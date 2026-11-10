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
//! - **items**: repels, bursts, decoys, thors and bricks used when the ship
//!   can't hold one. The server keeps an upper bound per item: what the ship
//!   spawns with (each item's maximum when `InitialBounty` hands out random
//!   prizes the server can't replay), plus prizes it saw (greens, `?prize`;
//!   a multiprize makes every item unknown again), less the uses it saw.
//!   Rockets are never seen in use, so they aren't counted; a portal is
//!   spent by a flash jump that nothing else explains (below).
//!
//! **Physics checks** (`MotionChecks`, on when the module is enabled), using
//! `oss-sim`'s validation (the same physics the client runs): between two
//! position packets of a player in a ship, the distance travelled, reported
//! speed, turn, path through walls, energy (after the weapons the server saw
//! fired) and stealth/cloak/X-radar/antiwarp bits must be possible for that
//! ship. The elapsed time a client claims is capped by oss-sim's
//! `ClientClock` (how far its timestamps have ever run ahead of the server's
//! clock, plus `MotionClockSlack` ticks, default 30), so a client can't widen
//! the bounds by skewing its timestamps, while packets bunched after a lag
//! spike are still judged fairly. That slack is per packet, so on its own a
//! client could claim it again on every packet — ten packets a second of it
//! is three seconds of free time a second. Claimed time beyond what the
//! server's own clock saw is therefore spent from a budget:
//! `MotionClockBudget` ticks (default 300, three seconds), refilled at
//! `MotionClockRefill` ticks a second (default 10, which is twice the clock
//! check's tolerance). A backlog after a lag spike spends from it once; a
//! client running fast spends on every packet and soon has the bounds
//! measured by the server's clock alone. This is what catches a short burst
//! of clock speed that the clock check's long window averages away. Energy is judged
//! between consecutive packets and, to count the shots fired, over spans of
//! at least three seconds. Teleport, speed and rotation
//! count only `MotionMargin` percent past their bound, energy `EnergyMargin`
//! percent of maximum energy past it. Jumps the server caused or can't see
//! are excused: respawns, ship changes, `?warpto`, warp prizes and turrets;
//! refills after deaths, prizes and ship resets; enemy repels nearby; free
//! shots under Super.
//!
//! **The flash bit buys one portal, not a free pass.** The client sets it
//! itself, so the motion is judged with it cleared and the bit can only
//! excuse a jump that would otherwise count. Such a jump is excused when the
//! energy matches a warp (oss-sim's signature) *and* the ship came out in a
//! spawn area (`Spawn:TeamN-X/Y/Radius`, else what `Radar:RadarMode` implies,
//! else the middle of the map within `Misc:WarpRadiusLimit`); failing that it
//! spends one of the portals the server's own item bound says the ship may
//! hold, and counts once the ship is out of portals until it picks up more.
//! A ship can therefore make at most `PortalMax` unexplained flash jumps
//! between prize pickups, rather than unlimited ones. Every excused jump is
//! logged (`target: "anticheat"`, info) even though it costs no trust, so a
//! wall hack that spends a portal a hop still leaves a trail.
//!
//! **Two kinds of check.** All eleven feed one score, but they are not one
//! kind of evidence, and which kind a check is decides what its weight
//! should be.
//!
//! - *Noise checks* — fire rate, flood, speed, rotation, energy, clock and
//!   weapon level — fire on things an honest player on a bad connection
//!   really does produce: a reordered packet, a burst delivered late, a
//!   clock a few percent fast. Their low weights are a **tolerance**,
//!   chosen so that a player who trips one now and then never drifts
//!   towards a spec over a long session. They should stay low.
//! - *Impossible checks* — wall, status, item, and a flash teleport nothing
//!   explained — fire on things no honest client does at all. A ship does
//!   not cross a wall; a client does not set a cloak bit it was never
//!   given; a ship does not spend a repel it never held; and a flash jump
//!   has already been offered every excuse the server has (warp signature
//!   and spawn area, then a portal from the ship's own bound) before it
//!   counts. Their weight is not a tolerance: whatever they cost short of
//!   the whole score is a **concession**.
//!
//! **Trust.** Each finding lowers the player's trust (100 = clean) by its
//! check's weight and raises [`CheatSuspected`]. Trust recovers by
//! `RecoverPerMinute` (default 10).
//!
//! There is **one pool and one recovery rate**, so no weight is a local
//! edit. Raising one check's weight raises what every *other* check costs
//! in practice, because the pool they all draw on refills no faster, and a
//! player near the floor is specced by whichever check happens to fire
//! next. A check of weight W costs nothing on average below one finding
//! per W / RecoverPerMinute minutes — weight 10 is one a minute, weight 50
//! one every five. The 5-second cooldown caps the other end.
//!
//! The impossible checks are weighted 50, which forgives exactly one. With
//! the default `SpecTrust` 40 and `KickTrust` 10, a single finding leaves
//! trust at 50 and the player keeps flying; two cost 100 and leave trust at
//! 0, which is below `KickTrust`, so under `Action = 2` the second one is a
//! kick rather than a spec. 50 is the top of the band that still forgives
//! one: 61 or more specs on the first finding, 91 or more kicks on it.
//!
//! That is deliberately a **midway** position. The end state for a check
//! nothing honest can produce is that it should not recover at all — a
//! player who crossed a wall an hour ago still crossed a wall. These still
//! expire only as insurance against our own detectors being wrong, which
//! is not a theoretical worry: the flash bit used to disable the wall check
//! for a player's whole session, and the map-checksum key was divided
//! signed where the client divides it unsigned. Both were ours, and both
//! would have convicted honest players. The case for letting an impossible
//! finding expire runs out when there is a false-positive rate measured on
//! real traffic — not before, and not on argument alone.
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
use oss_sim::{
    ClientClock, Context, Finding, Fired, Limits, MotionMap, Sample, TileMap, Tolerance,
};

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
    /// Used an item (repel, burst, decoy, thor, brick) the ship can't hold.
    Item,
}

const CHECKS: usize = 11;

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
            Check::Item => "item",
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
    /// Ticks of claimed time, beyond the server's own, a client may build up.
    pub clock_budget: u32,
    /// Ticks of that budget given back per second.
    pub clock_refill: u32,
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
                // this array is indexed by `Check::index()`, so the order is
                // the enum's, not a grouping. `I` marks the impossible
                // checks, weighted so that two findings cost a ship; the
                // rest are noise checks with a deliberate floor (module docs)
                i("ClockWeight", 30),
                i("FireRateWeight", 10),
                i("WeaponLevelWeight", 25),
                i("FloodWeight", 10),
                i("TeleportWeight", 50), // I
                i("SpeedWeight", 20),
                i("RotationWeight", 10),
                i("WallWeight", 50), // I
                i("EnergyWeight", 15),
                i("StatusWeight", 50), // I
                i("ItemWeight", 50),   // I
            ],
            clock_window: u64::try_from(i("ClockWindow", 20)).unwrap_or(20).max(5) * 100,
            clock_windows: u("ClockWindows", 2).max(1),
            clock_drift: f64::from(i("ClockDrift", 5).max(1)) / 100.0,
            fire_slack: u("FireDelaySlack", 2),
            max_positions: u("MaxPositionsPerSecond", 100),
            motion: i("MotionChecks", 1) != 0,
            motion_margin: u("MotionMargin", 25),
            energy_margin: u("EnergyMargin", 10),
            clock_slack: u("MotionClockSlack", 30),
            clock_budget: u("MotionClockBudget", 300),
            clock_refill: u("MotionClockRefill", 10),
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
    /// where a warp to spawn may land
    spawns: SpawnAreas,
    /// spawning hands out random prizes the server can't replay
    random_spawn: bool,
    items_max: oss_sim::Items,
}

/// Where a ship warped to spawn may come out: the arena's spawn areas, in
/// tiles, as the client works them out (`Spawn:TeamN-X/Y/Radius`, else the
/// halves or quarters `Radar:RadarMode` implies, else the middle of the map
/// within `Misc:WarpRadiusLimit`).
///
/// Any team's area counts, not just the player's: which freq spawns where
/// depends on the radar mode and on how many areas the zone set, and being
/// generous about *which* spawn only matters when the energy already looks
/// like a warp.
#[derive(Debug, Clone, Copy)]
struct SpawnAreas {
    /// centre x, centre y, radius, in tiles
    areas: [(i32, i32, i32); 4],
    n: usize,
}

impl SpawnAreas {
    fn new(cs: &oss_proto::ClientSettings, s: &oss_sim::ArenaSettings) -> Self {
        let mut areas = [(0, 0, 0); 4];
        let mut n = 0;
        let middle = |v: u16| if v == 0 { 512 } else { i32::from(v) };
        for sp in &cs.spawns {
            if sp.x() != 0 || sp.y() != 0 || sp.radius() != 0 {
                areas[n] = (middle(sp.x()), middle(sp.y()), i32::from(sp.radius()));
                n += 1;
            }
        }
        if n > 0 {
            return Self { areas, n };
        }
        // No spawn area set: the client puts each team at the edge of its
        // half or quarter of the map, by radar mode.
        let side = |bit: i32| (bit & 1) * 0x300 + 0x80;
        match cs.radar_mode() {
            1 | 3 => {
                areas[0] = (side(0), 0x180, 0x80);
                areas[1] = (side(1), 0x180, 0x80);
                n = 2;
            }
            2 | 4 => {
                for (i, a) in areas.iter_mut().enumerate() {
                    let f = i as i32;
                    *a = (side(f), side(f / 2), 0x80);
                }
                n = 4;
            }
            _ => {
                let r = s.warp_radius_limit.clamp(3, 512) as i32;
                areas[0] = (512, 512, r);
                n = 1;
            }
        }
        Self { areas, n }
    }

    /// Whether a point (in pixels) is inside a spawn area, with `slack` tiles
    /// of room for the client's search for a free tile near the spawn.
    fn contains(&self, x: i32, y: i32, slack: i32) -> bool {
        let (tx, ty) = (x.div_euclid(16), y.div_euclid(16));
        self.areas[..self.n].iter().any(|&(cx, cy, r)| {
            let (dx, dy) = (i64::from(tx - cx), i64::from(ty - cy));
            let reach = i64::from(r + slack);
            dx * dx + dy * dy <= reach * reach
        })
    }
}

/// Tiles of room allowed around a spawn area when judging a warp to spawn:
/// the client looks for a free tile near the spawn point and takes the first
/// one it finds, so landing a little outside is normal.
const SPAWN_SLACK: i32 = 16;

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
    /// last sample the physics checks accepted
    prev: Option<Sample>,
    /// when that sample arrived, by the server's clock
    prev_at: Option<Ticks>,
    /// claimed time the client may still run ahead of the server's clock
    lead_budget: i64,
    /// the player's clock as the server has seen it run
    clock: ClientClock,
    /// older sample the energy check spans from, the cheapest cost of the
    /// shots seen strictly after it, and whether energy was refilled since
    anchor: Option<Sample>,
    /// when that sample arrived, by the server's clock
    anchor_at: Option<Ticks>,
    anchor_spent: i32,
    anchor_refilled: bool,
    jumped: bool,
    refilled: bool,
    super_until: Option<Ticks>,
    /// upper bound on what the ship holds (items, capabilities, levels)
    bound: Option<oss_sim::Loadout>,
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
        spawns: SpawnAreas::new(&cs, &s),
        random_spawn: ss.initial_bounty > 0 && s.prize_weight_total() > 0,
        items_max: oss_sim::Items {
            repels: ss.repel_max,
            bursts: ss.burst_max,
            decoys: ss.decoy_max,
            thors: ss.thor_max,
            bricks: ss.brick_max,
            rockets: ss.rocket_max,
            portals: ss.portal_max,
        },
    })
}

/// The bound a ship spawns with: exact when spawning is deterministic,
/// otherwise every item at its maximum.
fn spawn_bound(lim: &ShipLimits) -> oss_sim::Loadout {
    let mut l = oss_sim::Loadout::initial(&lim.settings);
    if lim.random_spawn {
        l.items = lim.items_max;
    }
    l
}

/// The player's ship limits (cached per ship and arena) and item bound.
fn ensure_limits(zone: &mut Zone, p: PlayerId) -> Option<ShipLimits> {
    let pl = zone.players.get(p)?;
    let (arena, ship) = (pl.arena?, pl.ship);
    if ship.is_spec() {
        return None;
    }
    let cached = pl
        .ext
        .get::<Ac>()
        .and_then(|a| a.limits)
        .filter(|l| l.ship == ship && l.arena == arena);
    let lim = match cached {
        Some(l) => l,
        None => limits_for(zone, p, arena, ship)?,
    };
    let a = state(zone, p)?;
    a.limits = Some(lim);
    if a.bound.is_none() {
        a.bound = Some(spawn_bound(&lim));
    }
    Some(lim)
}

/// Item use: a finding if the bound says the ship can't hold one, otherwise
/// one fewer.
fn use_item(zone: &mut Zone, p: PlayerId, arena: ArenaId, cfg: &AcConfig, item: &'static str) {
    let empty = {
        let Some(b) = state(zone, p).and_then(|a| a.bound.as_mut()) else {
            return;
        };
        let n = match item {
            "repel" => &mut b.items.repels,
            "burst" => &mut b.items.bursts,
            "decoy" => &mut b.items.decoys,
            "thor" => &mut b.items.thors,
            _ => &mut b.items.bricks,
        };
        if *n == 0 {
            true
        } else {
            *n -= 1;
            false
        }
    };
    if empty {
        found(
            zone,
            p,
            arena,
            cfg,
            Check::Item,
            format!("used a {item} the ship can't hold"),
        );
    }
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
    let Some(lim) = ensure_limits(zone, p) else {
        return;
    };
    match pos.weapon.kind() {
        weapon_type::REPEL => use_item(zone, p, arena, &cfg, "repel"),
        weapon_type::BURST => use_item(zone, p, arena, &cfg, "burst"),
        weapon_type::DECOY => use_item(zone, p, arena, &cfg, "decoy"),
        weapon_type::THOR => use_item(zone, p, arena, &cfg, "thor"),
        _ => {}
    }
    // Flag:Flagger* upgrades apply to flag carriers, and to ball carriers
    // when Soccer:UseFlagger is set
    let carrying_ball = lim.use_flagger
        && super::balls::balls(zone, arena)
            .iter()
            .any(|b| b.carrier == Some(p));
    let flagger = flags > 0 || carrying_ball;

    let fired = weapon_checks(zone, p, arena, &cfg, &lim, &pos, flagger);

    // a turret moves with its host, which may be faster: check the host
    if cfg.motion && attached.is_none() {
        motion_checks(zone, p, arena, &cfg, &lim, &pos, freq, now, fired, flagger);
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

/// What a shot costs in energy points. A flag or ball carrier may pay the
/// flagger price instead, if lower. (oss-sim's `least_fire_cost` gives
/// everyone the lower of the two, for servers that don't know who carries
/// what; this one does, and with `Flag:FlaggerFireCostPercent` unset the
/// flagger price is 0, which would make every shot free.)
fn shot_cost(lim: &ShipLimits, fired: Fired, flagger: bool) -> i32 {
    let cost = oss_sim::fire_cost(&lim.settings, fired);
    let cost = if flagger {
        cost.min(oss_sim::flagger_cost(cost, lim.fire_cost_percent))
    } else {
        cost
    };
    cost / 1000
}

/// Energy is judged over at least this many ticks (3 s), from an older anchor
/// sample, so the shots fired in between can be counted.
const ENERGY_SPAN: u32 = 300;

/// Spends one of the portals the server believes the ship may still hold.
/// `None` when it can hold none; `Some(left)` when it could, with what is
/// left after (`Some(None)` when the server has lost count of the ship's
/// items — a multiprize — where every other item check is permissive too).
fn spend_portal(zone: &mut Zone, p: PlayerId) -> Option<Option<u8>> {
    let a = state(zone, p)?;
    match a.bound.as_mut() {
        None => Some(None),
        Some(b) if b.items.portals > 0 => {
            b.items.portals -= 1;
            Some(Some(b.items.portals))
        }
        Some(_) => None,
    }
}

/// Evidence for a flash-bit jump that the motion check would otherwise
/// count. The client sets the bit itself, so it buys nothing on its own:
/// either the energy matches a warp *and* the ship came out in a spawn area,
/// or the jump costs one of the portals the ship may hold. `None` when
/// nothing backs the claim, and the jump counts.
fn flash_excuse(
    zone: &mut Zone,
    p: PlayerId,
    lim: &ShipLimits,
    map: &MotionMap<'_>,
    prev: &Sample,
    next: &Sample,
    context: &Context,
) -> Option<String> {
    // the same pair as the motion check, but with the client's flash bit:
    // `flash_explained` is then oss-sim's warp-energy signature
    let v = oss_sim::check_motion(&lim.motion, map, prev, next, context, &Tolerance::default());
    if v.flash_explained && lim.spawns.contains(next.x, next.y, SPAWN_SLACK) {
        return Some("a warp to spawn".to_owned());
    }
    Some(match spend_portal(zone, p)? {
        Some(left) => format!("a portal, {left} left"),
        None => "items the server has lost count of".to_owned(),
    })
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
    fired: Option<Fired>,
    flagger: bool,
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
    let server_now = now.0 as u32;
    let free_shots = state(zone, p)
        .and_then(|a| a.super_until)
        .is_some_and(|t| now < t);
    let shot = match fired {
        Some(f) if !free_shots => shot_cost(lim, f, flagger),
        _ => 0,
    };
    let Some(a) = state(zone, p) else { return };
    let Some(prev) = a.prev else {
        a.clock.observe(next.time, server_now);
        a.prev = Some(next);
        a.prev_at = Some(now);
        a.lead_budget = i64::from(cfg.clock_budget);
        a.anchor = Some(next);
        a.anchor_at = Some(now);
        a.anchor_spent = 0;
        a.anchor_refilled = false;
        a.jumped = false;
        a.refilled = false;
        return;
    };
    let dt = next.time.wrapping_sub(prev.time);
    if dt == 0 {
        // a duplicate
        return;
    }
    let reordered = dt >= 0x8000_0000;
    // the cap comes from the client's own clock as the server has seen it
    // run (tolerates packets bunched after a lag spike); computed before
    // this packet is observed
    let max_ticks = a.clock.max_ticks(prev.time, server_now, cfg.clock_slack);
    let span_cap = a.anchor.map_or(0, |an| {
        let by_clock = a.clock.max_ticks(an.time, server_now, cfg.clock_slack);
        // the same budget over the whole span: claimed time may run ahead
        // of the server's by at most the budget, however it is divided up
        let gap = u32::try_from(a.anchor_at.map_or(0, |t| now.since(t))).unwrap_or(u32::MAX);
        by_clock.min(gap.saturating_add(cfg.clock_budget))
    });
    // That slack is granted on every packet, so on its own it would let a
    // client claim it again and again and keep a burst of fast clock inside
    // the bounds. Claimed time beyond the server's own elapsed time is
    // spent from a budget instead: enough for the backlog after a lag
    // spike, not enough to run fast for long.
    let max_ticks = if reordered {
        max_ticks
    } else {
        let gap = u32::try_from(a.prev_at.map_or(0, |t| now.since(t))).unwrap_or(u32::MAX);
        let refill = i64::from(gap) * i64::from(cfg.clock_refill) / 100;
        a.lead_budget = (a.lead_budget + refill).clamp(0, i64::from(cfg.clock_budget));
        let spend = i64::from(dt.saturating_sub(gap)).min(a.lead_budget);
        a.lead_budget -= spend;
        max_ticks.min(gap.saturating_add(u32::try_from(spend).unwrap_or(0)))
    };
    a.clock.observe(next.time, server_now);
    let (jumped, refilled) = (a.jumped, a.refilled);
    let anchor = a.anchor;
    let (anchor_spent, anchor_refilled) = (a.anchor_spent, a.anchor_refilled || refilled);
    if !reordered {
        a.prev = Some(next);
        a.prev_at = Some(now);
        a.jumped = false;
        a.refilled = false;
    }
    let repelled = repelled_near(zone, arena, lim, freq, [(prev.x, prev.y), (next.x, next.y)]);
    let Some(map) = motion_map(zone, arena) else {
        return;
    };
    let context = Context {
        jumped,
        repelled,
        refilled,
        // shots in this packet or the previous one may or may not be in
        // their energy readings, so none count between consecutive samples
        spent: 0,
        max_ticks,
        // the flash bit is judged here, not by oss-sim: the server has to
        // spend a portal for it (see `flash_excuse`)
        portal_held: false,
    };
    // The client sets the flash bit itself, so the motion is judged with it
    // cleared first; it can then only excuse a jump that needs excusing.
    let flashed = next.status & oss_proto::position::status::FLASH != 0;
    let honest = Sample {
        status: next.status & !oss_proto::position::status::FLASH,
        ..next
    };
    let verdict = oss_sim::check_motion(
        &lim.motion,
        &map,
        &prev,
        &honest,
        &context,
        &Tolerance::default(),
    );
    let over = 100 + cfg.motion_margin;
    // A jump worth excusing: one that would otherwise count against the
    // player. A shorter one costs nothing, so it costs no portal either.
    let jump_counts = |f: &Finding| match *f {
        Finding::Teleport { percent, .. } => percent > over,
        Finding::ThroughWall | Finding::InsideWall => true,
        _ => false,
    };
    let excuse = if flashed && verdict.findings().any(|f| jump_counts(&f)) {
        flash_excuse(zone, p, lim, &map, &prev, &next, &context)
    } else {
        None
    };
    if let Some(e) = &excuse {
        let name = zone
            .players
            .get(p)
            .map(|x| x.name.clone())
            .unwrap_or_default();
        // Excused, but never silent: a wall hack that spends portals for
        // every hop is still a pattern, and this is where it shows.
        tracing::info!(
            target: "anticheat",
            pid = p.0,
            %name,
            x = next.x,
            y = next.y,
            "flash jump excused: {e}",
        );
    }
    let mut findings: Vec<(Check, String)> = Vec::new();
    for f in verdict.findings() {
        // An excused jump explains the distance, the wall it crossed and
        // the heading it came out on, and nothing else.
        if excuse.is_some() && (jump_counts(&f) || matches!(f, Finding::Rotation { .. })) {
            continue;
        }
        let found = match f {
            Finding::Teleport { percent, .. } if percent > over => Some((
                Check::Teleport,
                if flashed {
                    format!(
                        "jumped {percent}% of the ship's reach with the flash bit but no portal or warp"
                    )
                } else {
                    format!(
                        "moved {percent}% of the furthest the ship could go in {} ticks",
                        verdict.ticks
                    )
                },
            )),
            Finding::Speed { percent } if percent > over => Some((
                Check::Speed,
                format!("reported {percent}% of the ship's top speed"),
            )),
            Finding::Rotation { percent } if percent > over => Some((
                Check::Rotation,
                format!("turned {percent}% of the most the ship can"),
            )),
            Finding::ThroughWall => Some((Check::Wall, "passed through a wall".to_owned())),
            Finding::InsideWall => Some((Check::Wall, "inside a wall".to_owned())),
            Finding::Energy { excess, percent } if percent > cfg.energy_margin => Some((
                Check::Energy,
                format!(
                    "{excess} more energy than the ship could have ({percent}% of its maximum)"
                ),
            )),
            Finding::Status { bits } => Some((
                Check::Status,
                format!("status bits {bits:#04x} without the capability"),
            )),
            _ => None,
        };
        findings.extend(found);
    }

    // Energy over a longer span, counting the shots the server saw strictly
    // between the anchor and this packet: an endless-energy client reports
    // full energy while firing, which no single pair can show.
    if !reordered && let Some(anchor) = anchor {
        let span = next.time.wrapping_sub(anchor.time);
        if (ENERGY_SPAN..0x8000_0000).contains(&span) {
            let ctx = Context {
                // only energy is judged over the span
                jumped: true,
                repelled: false,
                refilled: anchor_refilled,
                spent: anchor_spent,
                max_ticks: span_cap,
                // only energy is judged here, and `jumped` already waives
                // everything the flash bit could
                portal_held: false,
            };
            let v = oss_sim::check_motion(
                &lim.motion,
                &map,
                &anchor,
                &next,
                &ctx,
                &Tolerance::default(),
            );
            for f in v.findings() {
                if let Finding::Energy { excess, percent } = f
                    && percent > cfg.energy_margin
                {
                    findings.push((
                        Check::Energy,
                        format!(
                            "{excess} more energy than the ship could have after the shots fired over {span} ticks ({percent}% of its maximum)"
                        ),
                    ));
                }
            }
            if let Some(a) = state(zone, p) {
                a.anchor = Some(next);
                a.anchor_at = Some(now);
                a.anchor_spent = 0;
                a.anchor_refilled = false;
            }
        } else if let Some(a) = state(zone, p) {
            // this packet's own shot lies strictly between the anchor and
            // whatever sample closes the span
            a.anchor_spent = a.anchor_spent.saturating_add(shot);
            a.anchor_refilled |= refilled;
        }
    }
    for (check, detail) in findings {
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

/// A respawn (or `?shipreset`): the ship holds what it spawns with again.
pub fn respawned(zone: &mut Zone, p: PlayerId) {
    if let Some(a) = zone.players.get_mut(p).and_then(|x| x.ext.get_mut::<Ac>()) {
        a.bound = a.limits.as_ref().map(spawn_bound);
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
    if let Some(lim) = ensure_limits(zone, p)
        && let Some(b) = state(zone, p).and_then(|a| a.bound.as_mut())
    {
        if prize.abs() == PRIZE_MULTI {
            // random extra prizes: anything may be held now
            b.items = lim.items_max;
        } else if let Some(pz) = u8::try_from(prize.unsigned_abs())
            .ok()
            .and_then(oss_sim::Prize::from_id)
        {
            b.apply(&lim.settings, pz, prize < 0);
        }
    }
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
                a.bound = None;
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
                a.bound = None;
            }
        });
        // deaths respawn the ship somewhere else with full energy
        zone.on::<super::game::KillAnnounced>(me, Scope::Global, |z, ev| {
            excuse(z, ev.killed, Excuse::Both);
            respawned(z, ev.killed);
        });
        zone.on_packet(me, c2s::kind::DROP_BRICK, |z, p, _| {
            let Some(arena) = z.players.get(p).and_then(|x| x.arena) else {
                return;
            };
            if !enabled_in(z, arena) || exempt(z, p) || ensure_limits(z, p).is_none() {
                return;
            }
            if let Some(cfg) = arena_cfg(z, arena) {
                use_item(z, p, arena, &cfg, "brick");
            }
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
    fn weights_line_up_with_their_checks() {
        // `weights` is a positional array indexed by `Check::index()`, so a
        // reordered literal would silently give every check the wrong
        // weight. Pin the four that matter and one that must not move.
        let c = AcConfig::load(&crate::config::ConfigFile::default());
        for check in [Check::Wall, Check::Status, Check::Item, Check::Teleport] {
            assert_eq!(c.weights[check.index()], 50, "{}", check.name());
        }
        // noise checks keep their tolerant floor
        assert_eq!(c.weights[Check::Clock.index()], 30);
        assert_eq!(c.weights[Check::WeaponLevel.index()], 25);
        assert_eq!(c.weights[Check::FireRate.index()], 10);
        // one impossible finding is forgiven, two are not
        assert!(FULL_TRUST - 50 >= c.spec_trust);
        assert!(FULL_TRUST - 100 < c.kick_trust);
    }

    #[test]
    fn recovery_time() {
        assert_eq!(seconds_until(25, 40, 10), Some(90));
        assert_eq!(seconds_until(10, 40, 10), Some(180));
        assert_eq!(seconds_until(10, 40, 0), None);
    }
}
