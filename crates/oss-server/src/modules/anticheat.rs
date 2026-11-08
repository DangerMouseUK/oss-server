//! `anticheat`: server-side plausibility checks with a per-player trust score
//! (M6). An open client makes the exe checksum worthless, so the server has
//! to notice what a modified client does.
//!
//! **Off by default, and cautious when on.** `[AntiCheat] Enabled = 1` runs
//! the checks and logs what they find; nothing happens to players unless
//! `Action` is raised. A false auto-spec is worse than a missed cheat, so every
//! check allows for latency and jitter, each check counts at most once per
//! `Cooldown` seconds, and trust recovers over time.
//!
//! Checks (none of them simulate movement; see below):
//! - **clock** (speed hacks): over each `ClockWindow` seconds the client's
//!   position timestamps may run at most `ClockDrift` percent faster than the
//!   server's clock;
//! - **fire rate**: consecutive bullets, bombs or mines, measured on the
//!   client's own timestamps, may not come faster than the ship's
//!   `BulletFireDelay`/`MultiFireDelay`, `BombFireDelay` or
//!   `LandmineFireDelay`, less `FireDelaySlack` ticks;
//! - **weapon level**: guns or bombs above the ship's `MaxGuns`/`MaxBombs`
//!   (one more for flag carriers, who may get flagger upgrades);
//! - **flood**: more than `MaxPositionsPerSecond` position packets in a
//!   second.
//!
//! Each finding lowers the player's trust (100 = clean) by that check's
//! weight and raises [`CheatSuspected`]. Trust recovers by
//! `RecoverPerMinute`. With `Action = 1` a player whose trust falls below
//! `SpecTrust` is moved to spectator mode and can't take a ship until trust
//! is back above it; with `Action = 2` a player below `KickTrust` is also
//! kicked. Staff with `seemodchat` get a one-line notice for each action.
//! Players with `bypasssecurity` are exempt, as are bots.
//!
//! Ship limits come from the settings the player's client was actually sent,
//! read through `oss-sim`'s settings model. Movement, energy and item checks
//! will use `oss-sim`'s validation API (shared with the client, so both sides
//! judge physics identically) once oss-core provides it; see
//! `notes/server-needs-from-core.md`.

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::position::{C2SPosition, weapon_type};

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
}

impl Check {
    /// Short name for logs and `?trust`.
    pub fn name(self) -> &'static str {
        match self {
            Check::Clock => "clock",
            Check::FireRate => "fire rate",
            Check::WeaponLevel => "weapon level",
            Check::Flood => "flood",
        }
    }

    fn index(self) -> usize {
        self as usize
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
    /// Trust regained per minute.
    pub recover_per_minute: i32,
    /// A check counts at most once per this many ticks.
    pub cooldown: u64,
    /// Weights per [`Check`].
    pub weights: [i32; 4],
    /// Clock window (ticks).
    pub clock_window: u64,
    /// Allowed clock speed-up, percent.
    pub clock_drift: f64,
    /// Ticks forgiven on fire delays.
    pub fire_slack: u32,
    /// Position packets per second allowed.
    pub max_positions: u32,
}

impl AcConfig {
    /// Reads `[AntiCheat]`.
    pub fn load(cfg: &crate::config::ConfigFile) -> Self {
        let i = |k: &str, d: i32| cfg.get_int("AntiCheat", k, d);
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
            ],
            clock_window: u64::try_from(i("ClockWindow", 20)).unwrap_or(20).max(5) * 100,
            clock_drift: f64::from(i("ClockDrift", 5).max(1)) / 100.0,
            fire_slack: u32::try_from(i("FireDelaySlack", 2)).unwrap_or(2),
            max_positions: u32::try_from(i("MaxPositionsPerSecond", 100)).unwrap_or(100),
        }
    }
}

/// The limits of the ship a player is flying, from their settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShipLimits {
    arena: ArenaId,
    ship: Ship,
    bullet_delay: u32,
    bomb_delay: u32,
    mine_delay: u32,
    max_guns: u8,
    max_bombs: u8,
}

#[derive(Debug, Default)]
struct Ac {
    /// trust lost, in thousandths of a point (trust = 100 - lost)
    lost_milli: i64,
    limits: Option<ShipLimits>,
    /// (server tick, client time) at the start of the clock window
    clock_start: Option<(Ticks, u32)>,
    last_bullet: Option<u32>,
    last_bomb: Option<u32>,
    last_mine: Option<u32>,
    second_start: Ticks,
    second_count: u32,
    last_hit: [Option<Ticks>; 4],
    recent: Vec<(Ticks, Check, String)>,
    benched: bool,
}

fn state(zone: &mut Zone, p: PlayerId) -> Option<&mut Ac> {
    zone.players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<Ac>())
}

impl Ac {
    fn trust(&self) -> i32 {
        FULL_TRUST - ((self.lost_milli + 999) / 1000) as i32
    }
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
    let ship_settings = s.ships.get(usize::from(ship.0))?;
    Some(ShipLimits {
        arena,
        ship,
        bullet_delay: ship_settings
            .bullet_fire_delay
            .min(ship_settings.multi_fire_delay),
        bomb_delay: ship_settings.bomb_fire_delay,
        mine_delay: ship_settings.landmine_fire_delay,
        max_guns: ship_settings.max_guns,
        max_bombs: ship_settings.max_bombs,
    })
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
            message(
                zone,
                p,
                "You have been moved to spectator mode by the anti-cheat checks.",
            );
            tracing::warn!(target: "anticheat", pid = p.0, %name, trust, "specced");
            notify_staff(
                zone,
                &format!(
                    "[anticheat] {name} specced (trust {trust}; last: {}: {detail})",
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
                "[anticheat] {name} kicked (trust {trust}; last: {}: {detail})",
                check.name()
            ),
        );
        super::core::kick(zone, p);
    }
}

fn on_position(zone: &mut Zone, p: PlayerId, pkt: &[u8]) {
    let Some(pl) = zone.players.get(p) else {
        return;
    };
    let (Some(arena), PlayerStatus::Playing) = (pl.arena, pl.status) else {
        return;
    };
    let ship = pl.ship;
    let flags = pl.flags_carried;
    if !enabled_in(zone, arena) {
        return;
    }
    let Some(cfg) = arena_cfg(zone, arena) else {
        return;
    };
    if !cfg.enabled || exempt(zone, p) {
        return;
    }
    let Ok(pos) = C2SPosition::decode(pkt) else {
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

    // clock: client time against server time over a window
    let clock = {
        let Some(a) = state(zone, p) else { return };
        match a.clock_start {
            None => {
                a.clock_start = Some((now, pos.time));
                None
            }
            Some((s0, c0)) => {
                let server = now.since(s0);
                if server >= cfg.clock_window {
                    let client = u64::from(pos.time.wrapping_sub(c0));
                    a.clock_start = Some((now, pos.time));
                    let ratio = client as f64 / server as f64;
                    (ratio > 1.0 + cfg.clock_drift && client < server * 10).then_some(ratio)
                } else {
                    None
                }
            }
        }
    };
    if let Some(r) = clock {
        found(
            zone,
            p,
            arena,
            &cfg,
            Check::Clock,
            format!("client clock running {:.1}% fast", (r - 1.0) * 100.0),
        );
    }

    if ship.is_spec() {
        return;
    }
    let kind = pos.weapon.kind();
    if kind == weapon_type::NONE {
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
    let level = pos.weapon.level() + 1;
    let flagger = u8::from(flags > 0);
    let (class, max, delay) = match kind {
        weapon_type::BULLET | weapon_type::BOUNCE_BULLET => (0, lim.max_guns, lim.bullet_delay),
        weapon_type::BOMB | weapon_type::PROX_BOMB if pos.weapon.alternate() => {
            (2, lim.max_bombs, lim.mine_delay)
        }
        weapon_type::BOMB | weapon_type::PROX_BOMB => (1, lim.max_bombs, lim.bomb_delay),
        _ => return,
    };
    if level > max + flagger {
        found(
            zone,
            p,
            arena,
            &cfg,
            Check::WeaponLevel,
            format!("level {level} weapon (type {kind}) on a ship allowing {max}"),
        );
    }
    let gap = {
        let Some(a) = state(zone, p) else { return };
        let last = match class {
            0 => &mut a.last_bullet,
            1 => &mut a.last_bomb,
            _ => &mut a.last_mine,
        };
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
            &cfg,
            Check::FireRate,
            format!("fired {g} ticks after the last shot; the ship allows {delay}"),
        );
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
    let reset = inv.params.trim() == "-r";
    if reset {
        if let Some(a) = state(zone, t) {
            *a = Ac::default();
        }
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
            }
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
    }
}
