//! `anticheat`: server-side plausibility checks with a per-player trust score
//! (M6). An open client makes the exe checksum worthless, so the server has
//! to notice what a modified client does.
//!
//! **Off by default, and cautious when on.** `[AntiCheat] Enabled = 1` runs
//! the checks and logs what they find; nothing happens to players unless
//! `Action` is raised. A false auto-spec is worse than a missed cheat, so every
//! check allows for latency and jitter, each check counts at most once per
//! `Cooldown` seconds, and trust recovers over time. Every threshold below is
//! a setting.
//!
//! **Checks without physics:**
//! - **clock** (timer speed hacks): the client's position timestamps may run
//!   at most `ClockDrift` percent faster than the server's clock over each
//!   `ClockWindow` seconds; it counts once `ClockWindows` windows in a row
//!   drift, so one clock step or a single bad window never counts. A
//!   timestamp that went backwards (a resynchronised or wrapped clock) starts
//!   a new window instead. Accepted risk: a computer whose clock really runs
//!   that fast is indistinguishable from a slow timer hack; such a player is
//!   told plainly that their clock is fast.
//! - **fire rate**: consecutive bullets, bombs or mines, measured on the
//!   client's own timestamps, may not come faster than the ship's
//!   `BulletFireDelay`/`MultiFireDelay`, `BombFireDelay` or
//!   `LandmineFireDelay`, less `FireDelaySlack` ticks. A duplicated packet
//!   (same timestamp) is ignored.
//! - **weapon level**: guns or bombs above the ship's `MaxGuns`/`MaxBombs`,
//!   with one more allowed for flag carriers and, with `Soccer:UseFlagger`,
//!   ball carriers (the `Flag:Flagger*` upgrades).
//! - **flood**: more than `MaxPositionsPerSecond` position packets a second.
//!
//! **Physics checks** (`MotionChecks`, on when the module is enabled), using
//! `oss-sim`'s validation (the same physics the client runs): between two
//! position packets of a player in a ship, the distance travelled, reported
//! speed, turn, path through walls, energy (after the weapons the server saw
//! fired) and stealth/cloak/X-radar/antiwarp bits must be possible for that
//! ship. The elapsed time is capped by the server's own clock plus
//! `MotionClockSlack` ticks, so a client can't widen the bounds by skewing
//! its timestamps; this also catches short bursts of clock speed that the
//! clock check's long window averages away. Teleport, speed and rotation
//! count only `MotionMargin` percent past their bound, energy `EnergyMargin`
//! percent of maximum energy past it. Jumps the server caused or can't see
//! are excused: respawns, ship changes, `?warpto`, warp prizes, flashes
//! (warps, portals), turrets; refills after deaths, prizes and ship resets;
//! enemy repels nearby; free shots under Super.
//!
//! **Trust.** Each finding lowers the player's trust (100 = clean) by its
//! check's weight and raises [`CheatSuspected`]. Trust recovers by
//! `RecoverPerMinute` (default 10), and that number is the cheat budget: a
//! check of weight W costs nothing on average below one finding per
//! W / RecoverPerMinute minutes (weight 10: one a minute). The 5-second
//! cooldown caps the other end. It is a deliberate floor, so honest players
//! don't drift towards a spec over a long session.
//!
//! **Actions.** With `Action = 1` a player whose trust falls below
//! `SpecTrust` is moved to spectator mode and can't take a ship until trust
//! is back above it; with `Action = 2` a player below `KickTrust` is also
//! kicked. The player is told why, and staff with `seemodchat` get a notice
//! saying how long the player will be out, so they can use `?trust -r` to
//! override a false positive. Players with `bypasssecurity` are exempt, as
//! are bots.

use std::collections::HashMap;

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::position::{C2SPosition, weapon_type};
use oss_sim::{Context, Finding, Fired, Limits, MotionMap, Sample, TileMap, Tolerance};

use crate::arena::ArenaId;
use crate::commands::{Invocation, Target};
use crate::module::{Module, ModuleError};
use crate::player::{ClientKind, PlayerId, PlayerStatus, Ship};
use crate::registry::{Event, Owner, Scope};
use crate::time::Ticks;
use crate::zone::Zone;

use super::chat::message;

/// Highest trust.
pub const FULL_TRUST: i32 = 100;

/// What lets a player off every check.
pub const CAP_EXEMPT: &str = "bypasssecurity";

/// A check that can find something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Check {
    /// Client clock running fast (speed hack).
    Clock,
    /// Weapons fired faster than the ship allows.
    FireRate,
    /// Weapon level above the ship's maximum.
    WeaponLevel,
    /// Too many position packets.
    Flood,
    /// Moved further than the ship could.
    Teleport,
    /// Reported a speed the ship can't reach.
    Speed,
    /// Turned faster than the ship can.
    Rotation,
    /// Passed through or sat inside a wall.
    Wall,
    /// More energy than the ship could have.
    Energy,
    /// Stealth, cloak, X-radar or antiwarp without the capability.
    Status,
}

const CHECKS: usize = 10;

impl Check {
    /// Short name for logs and `?trust`.
    pub fn name(self) -> &'static str {
        match self {
            Check::Clock => "clock",
            Check::FireRate => "fire rate",
            Check::WeaponLevel => "weapon level",
            Check::Flood => "flood",
            Check::Teleport => "teleport",
            Check::Speed => "speed",
            Check::Rotation => "rotation",
            Check::Wall => "wall",
            Check::Energy => "energy",
            Check::Status => "status",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    /// What the player is told when this check moves them to spectator mode.
    fn player_reason(self, detail: &str) -> String {
        match self {
            Check::Clock => format!(
                "Your computer's clock is running fast ({detail}), which looks like a speed hack to the server. Fixing the clock fixes this; staff can help."
            ),
            c => format!(
                "You have been moved to spectator mode by the anti-cheat checks ({}).",
                c.name()
            ),
        }
    }
}

/// A check found something (raised whether or not any action follows).
#[derive(Debug, Clone)]
pub struct CheatSuspected {
    /// The player.
    pub player: PlayerId,
    /// Their arena.
    pub arena: ArenaId,
    /// Which check.
    pub check: Check,
    /// What it saw.
    pub detail: String,
    /// Trust after this finding.
    pub trust: i32,
}

impl Event for CheatSuspected {
    fn arena(&self) -> Option<ArenaId> {
        Some(self.arena)
    }
}

/// `[AntiCheat]` settings for an arena.
#[derive(Debug, Clone, PartialEq)]
pub struct AcConfig {
    /// Run the checks at all.
    pub enabled: bool,
    /// 0 = log only, 1 = spec, 2 = spec and kick.
    pub action: i32,
    /// Spec below this trust.
    pub spec_trust: i32,
    /// Kick below this trust.
    pub kick_trust: i32,
    /// Trust regained per minute (the cheat budget, see the module docs).
    pub recover_per_minute: i32,
    /// A check counts at most once per this many ticks.
    pub cooldown: u64,
    /// Weights per [`Check`].
    pub weights: [i32; CHECKS],
    /// Clock window (ticks).
    pub clock_window: u64,
    /// Drifting windows in a row before the clock check counts.
    pub clock_windows: u32,
    /// Allowed clock speed-up, percent.
    pub clock_drift: f64,
    /// Ticks forgiven on fire delays.
    pub fire_slack: u32,
    /// Position packets per second allowed.
    pub max_positions: u32,
    /// Run the physics checks.
    pub motion: bool,
    /// Percent past a movement bound before it counts.
    pub motion_margin: u32,
    /// Percent of maximum energy past the energy bound before it counts.
    pub energy_margin: u32,
    /// Ticks added to the server's elapsed time when capping the client's.
    pub clock_slack: u32,
}

impl AcConfig {
    /// Reads `[AntiCheat]`.
    pub fn load(cfg: &crate::config::ConfigFile) -> Self {
        let i = |k: &str, d: i32| cfg.get_int("AntiCheat", k, d);
        let u = |k: &str, d: i32| u32::try_from(i(k, d)).unwrap_or(0);
        Self {
            enabled: i("Enabled", 0) != 0,
            action: i("Action", 0).clamp(0, 2),
            spec_trust: i("SpecTrust", 40),
            kick_trust: i("KickTrust", 10),
            recover_per_minute: i("RecoverPerMinute", 10).max(0),
            cooldown: u64::try_from(i("Cooldown", 5)).unwrap_or(5) * 100,
            weights: [
                i("ClockWeight", 30),
                i("FireRateWeight", 10),
                i("WeaponLevelWeight", 25),
                i("FloodWeight", 10),
                i("TeleportWeight", 30),
                i("SpeedWeight", 20),
                i("RotationWeight", 10),
                i("WallWeight", 30),
                i("EnergyWeight", 15),
                i("StatusWeight", 25),
            ],
            clock_window: u64::try_from(i("ClockWindow", 20)).unwrap_or(20).max(5) * 100,
            clock_windows: u("ClockWindows", 2).max(1),
            clock_drift: f64::from(i("ClockDrift", 5).max(1)) / 100.0,
            fire_slack: u("FireDelaySlack", 2),
            max_positions: u("MaxPositionsPerSecond", 100),
            motion: i("MotionChecks", 1) != 0,
            motion_margin: u("MotionMargin", 25),
            energy_margin: u("EnergyMargin", 10),
            clock_slack: u("MotionClockSlack", 100),
        }
    }
}

/// What the server knows about a player's ship, from their settings.
#[derive(Debug, Clone, Copy)]
struct ShipLimits {
    arena: ArenaId,
    ship: Ship,
    bullet_delay: u32,
    bomb_delay: u32,
    mine_delay: u32,
    max_guns: u8,
    max_bombs: u8,
    use_flagger: bool,
    fire_cost_percent: u32,
    repel_time: u32,
    repel_distance: u32,
    settings: oss_sim::ShipSettings,
    motion: Limits,
}

/// Something the server did (or can't see) that excuses the next sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Excuse {
    /// The ship was moved: warp, respawn, `?warpto`.
    Jump,
    /// Energy was refilled: ship reset, prize, respawn.
    Refill,
    /// Both.
    Both,
}

#[derive(Debug, Default)]
struct Ac {
    /// trust lost, in thousandths of a point (trust = 100 - lost)
    lost_milli: i64,
    limits: Option<ShipLimits>,
    /// (server tick, client time) at the start of the clock window
    clock_start: Option<(Ticks, u32)>,
    clock_streak: u32,
    last_bullet: Option<u32>,
    last_bomb: Option<u32>,
    last_mine: Option<u32>,
    second_start: Ticks,
    second_count: u32,
    last_hit: [Option<Ticks>; CHECKS],
    recent: Vec<(Ticks, Check, String)>,
    benched: bool,
    /// last sample the physics checks accepted, with its arrival time
    prev: Option<(Sample, Ticks)>,
    /// energy spent on weapons since `prev`
    spent: i32,
    jumped: bool,
    refilled: bool,
    super_until: Option<Ticks>,
}

impl Ac {
    fn trust(&self) -> i32 {
        FULL_TRUST - ((self.lost_milli + 999) / 1000) as i32
    }
}

fn state(zone: &mut Zone, p: PlayerId) -> Option<&mut Ac> {
    zone.players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<Ac>())
}

/// A player's trust (100 = clean).
pub fn trust(zone: &Zone, p: PlayerId) -> i32 {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<Ac>())
        .map_or(FULL_TRUST, Ac::trust)
}

/// True while anti-cheat keeps the player out of ships.
pub fn no_ship(zone: &Zone, p: PlayerId) -> bool {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<Ac>())
        .is_some_and(|a| a.benched)
}

/// Tells the physics checks the server moved or refilled the player (call
/// from anything that does: staff commands, prizes, arena modules).
pub fn excuse(zone: &mut Zone, p: PlayerId, what: Excuse) {
    if let Some(a) = zone.players.get_mut(p).and_then(|x| x.ext.get_mut::<Ac>()) {
        a.jumped |= matches!(what, Excuse::Jump | Excuse::Both);
        a.refilled |= matches!(what, Excuse::Refill | Excuse::Both);
    }
}

/// `[AntiCheat]` cached per arena (re-read when the arena's config changes).
fn arena_cfg(zone: &Zone, a: ArenaId) -> Option<AcConfig> {
    let arena = zone.arenas.get(a)?;
    Some(
        arena
            .ext
            .get::<AcConfig>()
            .cloned()
            .unwrap_or_else(|| AcConfig::load(&arena.cfg)),
    )
}

fn enabled_in(zone: &Zone, a: ArenaId) -> bool {
    zone.arenas
        .get(a)
        .and_then(|x| x.ext.get::<AcConfig>())
        .is_some_and(|c| c.enabled)
}

fn limits_for(zone: &Zone, p: PlayerId, arena: ArenaId, ship: Ship) -> Option<ShipLimits> {
    let block = zone
        .iface::<dyn super::clientset::ClientSettings>()?
        .effective(zone, p)?;
    let cs = oss_proto::ClientSettings::decode(block.as_bytes()).ok()?;
    let s = oss_sim::ArenaSettings::from_client_settings(&cs);
    let ss = *s.ships.get(usize::from(ship.0))?;
    Some(ShipLimits {
        arena,
        ship,
        bullet_delay: ss.bullet_fire_delay.min(ss.multi_fire_delay),
        bomb_delay: ss.bomb_fire_delay,
        mine_delay: ss.landmine_fire_delay,
        max_guns: ss.max_guns,
        max_bombs: ss.max_bombs,
        use_flagger: s.soccer.use_flagger,
        fire_cost_percent: s.flagger.fire_cost_percent,
        repel_time: s.repel_time,
        repel_distance: s.repel_distance,
        settings: ss,
        motion: Limits::new(&ss, &s),
    })
}

/// Maps for the motion checks, by map checksum. A [`MotionMap`] borrows its
/// tile map and finds the wormholes once; each distinct map is kept for the
/// life of the process (about a megabyte each).
#[derive(Default)]
struct MotionMaps(HashMap<u32, MotionMap<'static>>);

fn motion_map(zone: &mut Zone, arena: ArenaId) -> Option<MotionMap<'static>> {
    let (crc, grid) = {
        let m = super::mapdata::arena_map(zone, arena)?;
        (m.crc, m.grid.clone())
    };
    let maps = zone.res.get_or_default::<MotionMaps>();
    if let Some(m) = maps.0.get(&crc) {
        return Some(m.clone());
    }
    let tiles: &'static TileMap = Box::leak(Box::new(TileMap::from_cells(grid.cells())));
    let m = MotionMap::new(tiles);
    maps.0.insert(crc, m.clone());
    Some(m)
}

fn exempt(zone: &mut Zone, p: PlayerId) -> bool {
    let bot = zone
        .players
        .get(p)
        .is_none_or(|x| !matches!(x.kind, ClientKind::Vie | ClientKind::Continuum));
    bot || zone
        .iface::<dyn super::capman::Capabilities>()
        .is_some_and(|c| c.has_capability(zone, p, CAP_EXEMPT))
}

fn notify_staff(zone: &mut Zone, text: &str) {
    let staff: Vec<PlayerId> = zone.players.ids();
    let caps = zone.iface::<dyn super::capman::Capabilities>();
    let to: Vec<PlayerId> = staff
        .into_iter()
        .filter(|&s| {
            caps.as_ref()
                .is_some_and(|c| c.has_capability(zone, s, super::capman::CAP_MOD_CHAT))
        })
        .collect();
    if let Some(c) = zone.iface::<dyn super::chat::Chat>() {
        c.send_any_message(
            zone,
            &to,
            oss_proto::common::chat_type::SERVER_ERROR,
            0,
            -1,
            text,
        );
    }
}

/// Seconds until trust climbs back to `target` at the configured rate.
fn seconds_until(trust: i32, target: i32, per_minute: i32) -> Option<i64> {
    (per_minute > 0).then(|| i64::from((target - trust).max(0)) * 60 / i64::from(per_minute))
}

/// Records a finding: lowers trust, logs, raises the event, maybe acts.
fn found(
    zone: &mut Zone,
    p: PlayerId,
    arena: ArenaId,
    cfg: &AcConfig,
    check: Check,
    detail: String,
) {
    let now = zone.now();
    let Some(a) = state(zone, p) else { return };
    if a.last_hit[check.index()].is_some_and(|t| now.since(t) < cfg.cooldown) {
        return;
    }
    a.last_hit[check.index()] = Some(now);
    a.lost_milli = (a.lost_milli + i64::from(cfg.weights[check.index()].max(0)) * 1000)
        .min(i64::from(FULL_TRUST) * 1000);
    a.recent.push((now, check, detail.clone()));
    if a.recent.len() > 10 {
        a.recent.remove(0);
    }
    let trust = a.trust();
    let name = zone
        .players
        .get(p)
        .map(|x| x.name.clone())
        .unwrap_or_default();
    tracing::warn!(target: "anticheat", pid = p.0, %name, check = check.name(), trust, "{detail}");
    zone.fire(&CheatSuspected {
        player: p,
        arena,
        check,
        detail: detail.clone(),
        trust,
    });
    if cfg.action >= 1 && trust < cfg.spec_trust {
        let in_ship = zone.players.get(p).is_some_and(|x| !x.ship.is_spec());
        if let Some(a) = state(zone, p) {
            a.benched = true;
        }
        if in_ship {
            let sf = zone
                .arenas
                .get(arena)
                .map_or(8025, super::arenaman::spec_freq);
            super::game::set_ship_and_freq(zone, p, Ship::SPEC, sf);
            message(zone, p, &check.player_reason(&detail));
            let out = seconds_until(trust, cfg.spec_trust, cfg.recover_per_minute)
                .map_or("until staff clear it".to_owned(), |s| {
                    format!("for about {s}s")
                });
            tracing::warn!(target: "anticheat", pid = p.0, %name, trust, "specced");
            notify_staff(
                zone,
                &format!(
                    "[anticheat] {name} specced, out {out} (trust {trust}; {}: {detail}). ?trust -r on them clears it.",
                    check.name()
                ),
            );
        }
    }
    if cfg.action >= 2 && trust < cfg.kick_trust {
        tracing::warn!(target: "anticheat", pid = p.0, %name, trust, "kicked");
        notify_staff(
            zone,
            &format!(
                "[anticheat] {name} kicked (trust {trust}; {}: {detail})",
                check.name()
            ),
        );
        super::core::kick(zone, p);
    }
}

/// The clock check: the client's elapsed time against the server's, over a
/// window. Returns the client's speed ratio when the drift has lasted
/// `ClockWindows` windows.
fn clock_check(a: &mut Ac, cfg: &AcConfig, now: Ticks, time: u32) -> Option<f64> {
    let Some((s0, c0)) = a.clock_start else {
        a.clock_start = Some((now, time));
        return None;
    };
    let server = now.since(s0);
    if server < cfg.clock_window {
        return None;
    }
    a.clock_start = Some((now, time));
    let client = time.wrapping_sub(c0);
    // A timestamp behind the window's start (a resynchronised clock, or
    // packets out of order across a wrap) reads as nearly 2^32: not a hack,
    // just start again. Any forward delta, however large, is judged.
    if client >= 0x8000_0000 {
        a.clock_streak = 0;
        return None;
    }
    let ratio = f64::from(client) / server as f64;
    if ratio > 1.0 + cfg.clock_drift {
        a.clock_streak += 1;
        (a.clock_streak >= cfg.clock_windows).then_some(ratio)
    } else {
        a.clock_streak = 0;
        None
    }
}

/// Recent repels in an arena: (when, x, y, freq).
#[derive(Default)]
struct Repels(Vec<(Ticks, i32, i32, u16)>);

fn repelled_near(
    zone: &Zone,
    arena: ArenaId,
    lim: &ShipLimits,
    freq: u16,
    pts: [(i32, i32); 2],
) -> bool {
    let now = zone.now();
    let window = u64::from(lim.repel_time) + 100;
    let reach = i64::from(lim.repel_distance) + 32;
    zone.arenas
        .get(arena)
        .and_then(|a| a.ext.get::<Repels>())
        .is_some_and(|r| {
            r.0.iter().any(|&(t, x, y, f)| {
                f != freq
                    && now.since(t) <= window
                    && pts.iter().any(|&(px, py)| {
                        let (dx, dy) = (i64::from(px - x), i64::from(py - y));
                        dx * dx + dy * dy <= reach * reach
                    })
            })
        })
}

fn on_position(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    let Some(pl) = zone.players.get(p) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (pl.arena, pl.status) else {
        return;
    };
    let (ship, freq, flags, attached) = (pl.ship, pl.freq, pl.flags_carried, pl.attached);
    if !enabled_in(zone, arena) {
        return;
    }
    let Ok(pos) = C2SPosition::decode(pkt) else {
        return;
    };
    // remember enemy repels for everyone's motion checks
    if pos.weapon.kind() == weapon_type::REPEL && !ship.is_spec() {
        let now = zone.now();
        if let Some(a) = zone.arenas.get_mut(arena) {
            let r = a.ext.get_or_default::<Repels>();
            r.0.retain(|(t, ..)| now.since(*t) < 3000);
            r.0.push((now, i32::from(pos.x), i32::from(pos.y), freq));
        }
    }
    if exempt(zone, p) {
        return;
    }
    let Some(cfg) = arena_cfg(zone, arena) else {
        return;
    };
    let now = zone.now();

    // flood
    let flood = {
        let Some(a) = state(zone, p) else { return };
        if now.since(a.second_start) >= 100 {
            a.second_start = now;
            a.second_count = 0;
        }
        a.second_count += 1;
        a.second_count == cfg.max_positions + 1
    };
    if flood {
        found(
            zone,
            p,
            arena,
            &cfg,
            Check::Flood,
            format!(
                "more than {} position packets in a second",
                cfg.max_positions
            ),
        );
    }

    // clock
    let clock = state(zone, p).and_then(|a| clock_check(a, &cfg, now, pos.time));
    if let Some(r) = clock {
        found(
            zone,
            p,
            arena,
            &cfg,
            Check::Clock,
            format!("{:.1}% ahead of the server's clock", (r - 1.0) * 100.0),
        );
    }

    if ship.is_spec() {
        return;
    }
    // the ship's limits, recomputed when ship or arena changes
    let cached = zone
        .players
        .get(p)
        .and_then(|x| x.ext.get::<Ac>())
        .and_then(|a| a.limits)
        .filter(|l| l.ship == ship && l.arena == arena);
    let Some(lim) = cached.or_else(|| limits_for(zone, p, arena, ship)) else {
        return;
    };
    if let Some(a) = state(zone, p) {
        a.limits = Some(lim);
    }
    // Flag:Flagger* upgrades apply to flag carriers, and to ball carriers
    // when Soccer:UseFlagger is set
    let carrying_ball = lim.use_flagger
        && super::balls::balls(zone, arena)
            .iter()
            .any(|b| b.carrier == Some(p));
    let flagger = flags > 0 || carrying_ball;

    if let Some(f) = weapon_checks(zone, p, arena, &cfg, &lim, &pos, flagger) {
        let free = state(zone, p)
            .and_then(|a| a.super_until)
            .is_some_and(|t| now < t);
        if !free {
            let mut cost = oss_sim::fire_cost(&lim.settings, f);
            if flagger {
                cost = oss_sim::flagger_cost(cost, lim.fire_cost_percent);
            }
            if let Some(a) = state(zone, p) {
                a.spent = a.spent.saturating_add(cost / 1000);
            }
        }
    }

    if cfg.motion && attached.is_none() {
        motion_checks(zone, p, arena, &cfg, &lim, &pos, freq, now);
    }
}

/// Weapon level and fire rate; returns what was fired, for the energy check.
fn weapon_checks(
    zone: &mut Zone,
    p: PlayerId,
    arena: ArenaId,
    cfg: &AcConfig,
    lim: &ShipLimits,
    pos: &C2SPosition,
    flagger: bool,
) -> Option<Fired> {
    let kind = pos.weapon.kind();
    let level = pos.weapon.level();
    let (class, max, delay, fired) = match kind {
        weapon_type::BULLET | weapon_type::BOUNCE_BULLET => (
            0,
            lim.max_guns,
            lim.bullet_delay,
            Fired::Bullet {
                level,
                multifire: pos.weapon.alternate(),
            },
        ),
        weapon_type::BOMB | weapon_type::PROX_BOMB if pos.weapon.alternate() => {
            (2, lim.max_bombs, lim.mine_delay, Fired::Mine { level })
        }
        weapon_type::BOMB | weapon_type::PROX_BOMB => {
            (1, lim.max_bombs, lim.bomb_delay, Fired::Bomb { level })
        }
        _ => return None,
    };
    let shown = level + 1;
    if shown > max + u8::from(flagger) {
        found(
            zone,
            p,
            arena,
            cfg,
            Check::WeaponLevel,
            format!("level {shown} weapon (type {kind}) on a ship allowing {max}"),
        );
    }
    let gap = {
        let a = state(zone, p)?;
        let last = match class {
            0 => &mut a.last_bullet,
            1 => &mut a.last_bomb,
            _ => &mut a.last_mine,
        };
        // a duplicated packet carries the same timestamp: not a second shot
        if *last == Some(pos.time) {
            return None;
        }
        let gap = last.map(|t| pos.time.wrapping_sub(t));
        *last = Some(pos.time);
        gap
    };
    if let Some(g) = gap
        && g < delay.saturating_sub(cfg.fire_slack)
        && g < 0x8000_0000
    {
        found(
            zone,
            p,
            arena,
            cfg,
            Check::FireRate,
            format!("fired {g} ticks after the last shot; the ship allows {delay}"),
        );
    }
    Some(fired)
}

#[allow(clippy::too_many_arguments)]
fn motion_checks(
    zone: &mut Zone,
    p: PlayerId,
    arena: ArenaId,
    cfg: &AcConfig,
    lim: &ShipLimits,
    pos: &C2SPosition,
    freq: u16,
    now: Ticks,
) {
    let next = Sample {
        time: pos.time,
        x: i32::from(pos.x),
        y: i32::from(pos.y),
        x_speed: i32::from(pos.x_speed),
        y_speed: i32::from(pos.y_speed),
        rotation: pos.rotation as u8,
        status: pos.status,
        energy: i32::from(pos.energy),
    };
    let Some(a) = state(zone, p) else { return };
    let Some((prev, prev_at)) = a.prev else {
        a.prev = Some((next, now));
        a.spent = 0;
        a.jumped = false;
        a.refilled = false;
        return;
    };
    // an older or duplicated packet: nothing to judge
    let dt = next.time.wrapping_sub(prev.time);
    if dt == 0 || dt >= 0x8000_0000 {
        return;
    }
    let (jumped, refilled, spent) = (a.jumped, a.refilled, a.spent);
    a.prev = Some((next, now));
    a.spent = 0;
    a.jumped = false;
    a.refilled = false;
    let server_ticks = u32::try_from(now.since(prev_at)).unwrap_or(u32::MAX);
    let repelled = repelled_near(zone, arena, lim, freq, [(prev.x, prev.y), (next.x, next.y)]);
    let Some(map) = motion_map(zone, arena) else {
        return;
    };
    let context = Context {
        jumped,
        repelled,
        refilled,
        spent,
        max_ticks: Some(server_ticks.saturating_add(cfg.clock_slack)),
    };
    let verdict = oss_sim::check_motion(
        &lim.motion,
        &map,
        &prev,
        &next,
        &context,
        &Tolerance::default(),
    );
    let over = 100 + cfg.motion_margin;
    for f in verdict.findings() {
        let (check, detail) = match f {
            // a flash means a warp or portal, which the server can't see
            Finding::Teleport { flash: true, .. } => continue,
            Finding::Teleport { percent, .. } if percent > over => (
                Check::Teleport,
                format!(
                    "moved {percent}% of the furthest the ship could go in {} ticks",
                    verdict.ticks
                ),
            ),
            Finding::Speed { percent } if percent > over => (
                Check::Speed,
                format!("reported {percent}% of the ship's top speed"),
            ),
            Finding::Rotation { percent } if percent > over => (
                Check::Rotation,
                format!("turned {percent}% of the most the ship can"),
            ),
            Finding::ThroughWall => (Check::Wall, "passed through a wall".to_owned()),
            Finding::InsideWall => (Check::Wall, "inside a wall".to_owned()),
            Finding::Energy { excess, percent } if percent > cfg.energy_margin => (
                Check::Energy,
                format!(
                    "{excess} more energy than the ship could have ({percent}% of its maximum)"
                ),
            ),
            Finding::Status { bits } => (
                Check::Status,
                format!("status bits {bits:#04x} without the capability"),
            ),
            _ => continue,
        };
        found(zone, p, arena, cfg, check, detail);
    }
}

fn recover(zone: &mut Zone) -> bool {
    for p in zone.players.ids() {
        let Some(a) = zone.players.get(p).and_then(|x| x.arena) else {
            continue;
        };
        let Some(cfg) = arena_cfg(zone, a) else {
            continue;
        };
        if let Some(st) = zone.players.get_mut(p).and_then(|x| x.ext.get_mut::<Ac>()) {
            // per second: a sixtieth of the per-minute amount, in milli-points
            st.lost_milli = (st.lost_milli - i64::from(cfg.recover_per_minute) * 1000 / 60).max(0);
            if st.benched && st.trust() >= cfg.spec_trust {
                st.benched = false;
                tracing::info!(target: "anticheat", pid = p.0, "trust recovered; may play again");
            }
        }
    }
    true
}

fn c_trust(zone: &mut Zone, inv: &Invocation<'_>) {
    let t = match inv.target {
        Target::Player(t) => t,
        _ => inv.sender,
    };
    let name = zone
        .players
        .get(t)
        .map(|x| x.name.clone())
        .unwrap_or_default();
    if inv.params.trim() == "-r" {
        if let Some(a) = state(zone, t) {
            *a = Ac::default();
        }
        tracing::info!(target: "anticheat", pid = t.0, %name, "trust reset by staff");
        message(
            zone,
            inv.sender,
            &format!("{name}: trust reset to {FULL_TRUST}"),
        );
        return;
    }
    let now = zone.now();
    let (benched, recent) = zone
        .players
        .get(t)
        .and_then(|x| x.ext.get::<Ac>())
        .map(|a| (a.benched, a.recent.clone()))
        .unwrap_or_default();
    let tr = trust(zone, t);
    let line = format!(
        "{name}: trust {tr}{}",
        if benched { " (kept in spec)" } else { "" }
    );
    message(zone, inv.sender, &line);
    for (at, check, detail) in recent.iter().rev().take(5) {
        let ago = now.since(*at) / 100;
        message(
            zone,
            inv.sender,
            &format!("  {ago}s ago: {}: {detail}", check.name()),
        );
    }
}

/// SubSpace prize numbers the physics checks care about.
const PRIZE_WARP: i32 = 7;
const PRIZE_SUPER: i32 = 17;
const PRIZE_MULTI: i32 = 25;

/// A prize changes what the next sample may show: any prize may refill
/// energy (full charge, or a capability the arena doesn't allow); a warp
/// moves the ship; Super makes shots free for up to `SuperTime`.
pub fn prize_taken(zone: &mut Zone, p: PlayerId, prize: i32) {
    let jump = prize == PRIZE_WARP || prize == PRIZE_MULTI;
    excuse(zone, p, if jump { Excuse::Both } else { Excuse::Refill });
    if prize == PRIZE_SUPER || prize == PRIZE_MULTI {
        let now = zone.now();
        let super_time = zone
            .players
            .get(p)
            .and_then(|x| x.ext.get::<Ac>())
            .and_then(|a| a.limits)
            .map_or(6000, |l| u64::from(l.settings.super_time) + 100);
        if let Some(a) = state(zone, p) {
            a.super_until = Some(Ticks(now.0 + super_time));
        }
    }
}

/// The `anticheat` module.
pub struct AntiCheatModule;

impl Module for AntiCheatModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on_packet(me, c2s::kind::POSITION, on_position);
        zone.on::<super::arenaman::ArenaAction>(me, Scope::Global, |z, ev| {
            use super::arenaman::ArenaActionKind;
            if matches!(
                ev.action,
                ArenaActionKind::Create | ArenaActionKind::ConfChanged
            ) && let Some(a) = z.arenas.get_mut(ev.arena)
            {
                let c = AcConfig::load(&a.cfg);
                a.ext.insert(c);
            }
        });
        zone.on::<super::game::ShipFreqChange>(me, Scope::Global, |z, ev| {
            if let Some(a) = z
                .players
                .get_mut(ev.player)
                .and_then(|x| x.ext.get_mut::<Ac>())
            {
                a.limits = None;
                a.last_bullet = None;
                a.last_bomb = None;
                a.last_mine = None;
                a.prev = None;
            }
        });
        zone.on::<super::core::PlayerAction>(me, Scope::Global, |z, ev| {
            if ev.action == super::core::Action::EnterArena
                && let Some(a) = z
                    .players
                    .get_mut(ev.player)
                    .and_then(|x| x.ext.get_mut::<Ac>())
            {
                a.limits = None;
                a.clock_start = None;
                a.clock_streak = 0;
                a.prev = None;
            }
        });
        // deaths respawn the ship somewhere else with full energy
        zone.on::<super::game::KillAnnounced>(me, Scope::Global, |z, ev| {
            excuse(z, ev.killed, Excuse::Both);
        });
        // prizes: warp moves, full charge refills, super makes shots free
        zone.on::<super::game::GreenPickup>(me, Scope::Global, |z, ev| {
            prize_taken(z, ev.player, i32::from(ev.prize));
        });
        zone.on::<super::game::Attach>(me, Scope::Global, |z, ev| {
            excuse(z, ev.player, Excuse::Jump);
        });
        zone.set_timer(me, 100, 100, recover);
        zone.add_command(
            me,
            "trust",
            None,
            Some(
                "Module: anticheat\nTargets: player or none\nArgs: [-r]\n\
                 Shows a player's anti-cheat trust and recent findings; -r resets them.\n",
            ),
            c_trust,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let c = AcConfig::load(&crate::config::ConfigFile::default());
        assert!(!c.enabled);
        assert_eq!(c.action, 0);
        assert!(c.spec_trust > c.kick_trust);
        assert!(c.clock_windows >= 2);
    }

    fn cfg() -> AcConfig {
        AcConfig {
            clock_window: 1000,
            clock_windows: 2,
            ..AcConfig::load(&crate::config::ConfigFile::default())
        }
    }

    #[test]
    fn clock_needs_a_streak_and_catches_blatant_hacks() {
        let c = cfg();
        let mut a = Ac::default();
        assert_eq!(clock_check(&mut a, &c, Ticks(0), 0), None);
        // one fast window doesn't count; two in a row do
        assert_eq!(clock_check(&mut a, &c, Ticks(1000), 1200), None);
        assert!(clock_check(&mut a, &c, Ticks(2000), 2400).is_some());
        // ten times faster is a hack too (it used to be ignored)
        let mut b = Ac::default();
        clock_check(&mut b, &c, Ticks(0), 0);
        clock_check(&mut b, &c, Ticks(1000), 10_000);
        assert!(clock_check(&mut b, &c, Ticks(2000), 20_000).unwrap_or(0.0) > 9.0);
        // a clock that jumped backwards just restarts
        let mut d = Ac::default();
        clock_check(&mut d, &c, Ticks(0), 5000);
        assert_eq!(clock_check(&mut d, &c, Ticks(1000), 100), None);
        assert_eq!(d.clock_streak, 0);
        // a forward wrap of the counter is ordinary time
        let mut e = Ac::default();
        clock_check(&mut e, &c, Ticks(0), u32::MAX - 500);
        assert_eq!(clock_check(&mut e, &c, Ticks(1000), 499), None);
    }

    #[test]
    fn recovery_time() {
        assert_eq!(seconds_until(25, 40, 10), Some(90));
        assert_eq!(seconds_until(10, 40, 10), Some(180));
        assert_eq!(seconds_until(10, 40, 0), None);
    }
}
