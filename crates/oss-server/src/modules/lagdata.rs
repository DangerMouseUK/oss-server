//! `lagdata` and `lagaction`: lag measurement and the actions taken on it
//! (ASSS `lagdata.c`, `lagaction.c`).
//!
//! Pings come from three places: position packets (one-way latency, doubled),
//! the client's own numbers in security responses, and the transport's
//! reliable round-trip time. Packet loss compares what one side sent with what
//! the other says it received (from the transport's counters and the client's
//! time-sync reports). Averages follow ASSS exactly: clamp to 0..10000, then
//! `avg = (avg * 7 + ping) / 8`.
//!
//! Weapon packet loss compares the weapon packets the server relayed to a
//! player (counted by `game`, sampled with each of their position packets)
//! with the weapon count their client reports in its security response.
//!
//! `lagaction` checks every `Lag:CheckInterval` ticks, in three tiers:
//! - over `Lag:PingToSpec`, `*LossToSpec` or `SpikeToSpec` (no packets for
//!   that long), players are moved to spectator mode and can't take a ship
//!   until a later check finds them under the limits;
//! - over `*ToDisallowFlags`, they can't pick up flags or balls;
//! - between `*ToStartIgnoringWeapons` and `*ToIgnoreAllWeapons` a matching
//!   share (the worst of ping, S2C loss and weapon loss) of their weapons is
//!   dropped by `game`.
//!
//! `?lag` shows a player's numbers.

use crate::commands::{Invocation, Target};
use crate::module::{Module, ModuleError};
use crate::player::{ClientKind, PlayerId, Ship};
use crate::registry::Owner;
use crate::zone::Zone;

use super::chat::message;

/// ASSS `PLOSS_MIN_PACKETS`.
pub const PLOSS_MIN_PACKETS: i64 = 200;

/// One ping series (ASSS `struct PingData`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PingStat {
    /// Last sample.
    pub current: i32,
    /// Running average.
    pub avg: i32,
    /// Lowest sample.
    pub min: i32,
    /// Highest sample.
    pub max: i32,
}

impl Default for PingStat {
    fn default() -> Self {
        Self {
            current: 0,
            avg: 0,
            min: i32::MAX,
            max: 0,
        }
    }
}

impl PingStat {
    /// ASSS `add_ping`.
    pub fn add(&mut self, ping: i32) {
        let ping = ping.clamp(0, 10000);
        self.current = ping;
        self.avg = (self.avg * 7 + ping) / 8;
        self.min = self.min.min(ping);
        self.max = self.max.max(ping);
    }
}

/// ASSS `QueryPLoss` for one direction: 0 until more than 200 packets were
/// sent; can be negative if the receiver over-reports (as in ASSS).
///
/// Deliberate difference: ASSS copies its unsigned counters into `int`
/// first, so a count above `i32::MAX` turns negative, fails the 200-packet
/// test and reads as no loss at all. One side of each ratio comes from the
/// client (its time-sync and security packets), so in ASSS a client can
/// escape every loss limit by claiming more than 2^31 packets. Here the
/// counts stay 64-bit: such a claim is taken at face value, reads as loss
/// near 100% and only gets the claimant specced. No honest client gets near
/// 2^31 packets (that is months of play).
pub fn ploss(sent: u64, received: u64) -> f64 {
    let (s, r) = (sent as i64, received as i64);
    if s > PLOSS_MIN_PACKETS {
        (s - r) as f64 / s as f64
    } else {
        0.0
    }
}

/// A player's lag numbers.
#[derive(Debug, Clone, Default)]
pub struct LagData {
    /// Position-packet ping.
    pub pping: PingStat,
    /// Client-reported ping.
    pub cping: PingStat,
    /// Reliable round-trip ping.
    pub rping: PingStat,
    /// Packets the server sent.
    pub s_sent: u64,
    /// Packets the server received.
    pub s_received: u64,
    /// Packets the client says it sent.
    pub c_sent: u64,
    /// Packets the client says it received.
    pub c_received: u64,
    /// Weapon packets relayed to the player, as of their last security report.
    pub wpn_sent: u64,
    /// Weapon packets the client says it received.
    pub wpn_received: u64,
    /// Weapon packets relayed to the player, as of their last position packet.
    pub last_wpn_sent: u64,
    /// Too lagged to touch flags and balls.
    pub no_flags_balls: bool,
    /// Too lagged to take a ship (ASSS `no_ship`).
    pub no_ship: bool,
    /// Share of the player's weapons being ignored (0 to 1).
    pub ignore_weapons: f64,
}

impl LagData {
    /// Server-to-client loss.
    pub fn s2c_loss(&self) -> f64 {
        ploss(self.s_sent, self.c_received)
    }

    /// Client-to-server loss.
    pub fn c2s_loss(&self) -> f64 {
        ploss(self.c_sent, self.s_received)
    }

    /// Server-to-client weapon packet loss.
    pub fn wpn_loss(&self) -> f64 {
        ploss(self.wpn_sent, self.wpn_received)
    }

    /// The weighted average lagaction uses: reliable ping counts twice.
    pub fn weighted_ping(&self) -> i32 {
        (self.pping.avg + self.cping.avg + 2 * self.rping.avg) / 4
    }
}

fn data(zone: &mut Zone, p: PlayerId) -> Option<&mut LagData> {
    zone.players
        .get_mut(p)
        .map(|x| x.ext.get_or_default::<LagData>())
}

/// A copy of a player's lag data.
pub fn lag_data(zone: &Zone, p: PlayerId) -> LagData {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<LagData>())
        .cloned()
        .unwrap_or_default()
}

/// A position packet: its latency (one way, ms) and how many weapon packets
/// had been relayed to the player by then.
pub fn position(zone: &mut Zone, p: PlayerId, one_way_ms: i32, wpn_sent: u64) {
    if let Some(d) = data(zone, p) {
        d.pping.add(one_way_ms * 2);
        d.last_wpn_sent = wpn_sent;
    }
}

/// A security response: the client's average ping (ms) and the number of
/// weapon packets it says it received.
pub fn client_ping(zone: &mut Zone, p: PlayerId, ms: i32, weapon_count: u32) {
    if let Some(d) = data(zone, p) {
        d.cping.add(ms);
        d.wpn_received = u64::from(weapon_count);
        d.wpn_sent = d.last_wpn_sent;
    }
}

/// Transport numbers: round trip and packet counters on both sides.
pub fn transport(
    zone: &mut Zone,
    p: PlayerId,
    rtt_ms: i32,
    sent: u64,
    received: u64,
    peer: Option<(u32, u32)>,
) {
    if let Some(d) = data(zone, p) {
        if rtt_ms > 0 {
            d.rping.add(rtt_ms);
        }
        d.s_sent = sent;
        d.s_received = received;
        if let Some((cs, cr)) = peer {
            d.c_sent = u64::from(cs);
            d.c_received = u64::from(cr);
        }
    }
}

/// True if lagaction has barred the player from taking a ship.
pub fn no_ship(zone: &Zone, p: PlayerId) -> bool {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<LagData>())
        .is_some_and(|d| d.no_ship)
}

/// True if lagaction has barred the player from flags and balls.
pub fn no_flags_balls(zone: &Zone, p: PlayerId) -> bool {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<LagData>())
        .is_some_and(|d| d.no_flags_balls)
}

/// lagaction's limits for an arena (ASSS `laglimits_t`). Losses are
/// fractions (the settings are in tenths of a percent).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// `Lag:PingToSpec`.
    pub ping_spec: i32,
    /// `Lag:PingToStartIgnoringWeapons`.
    pub ping_wpn_start: i32,
    /// `Lag:PingToIgnoreAllWeapons`.
    pub ping_wpn_all: i32,
    /// `Lag:PingToDisallowFlags`.
    pub ping_noflags: i32,
    /// `Lag:S2CLossToSpec`.
    pub s2c_spec: f64,
    /// `Lag:S2CLossToStartIgnoringWeapons`.
    pub s2c_wpn_start: f64,
    /// `Lag:S2CLossToIgnoreAllWeapons`.
    pub s2c_wpn_all: f64,
    /// `Lag:S2CLossToDisallowFlags`.
    pub s2c_noflags: f64,
    /// `Lag:WeaponLossToSpec`.
    pub wpn_spec: f64,
    /// `Lag:WeaponLossToStartIgnoringWeapons`.
    pub wpn_wpn_start: f64,
    /// `Lag:WeaponLossToIgnoreAllWeapons`.
    pub wpn_wpn_all: f64,
    /// `Lag:WeaponLossToDisallowFlags`.
    pub wpn_noflags: f64,
    /// `Lag:C2SLossToSpec`.
    pub c2s_spec: f64,
    /// `Lag:C2SLossToDisallowFlags`.
    pub c2s_noflags: f64,
    /// `Lag:SpikeToSpec` (ms without any packet).
    pub spike_spec: i32,
}

impl Limits {
    /// Reads an arena's `[Lag]` section with ASSS's defaults.
    pub fn load(cfg: &crate::config::ConfigFile) -> Self {
        let int = |k: &str, d: i32| cfg.get_int("Lag", k, d);
        let pct = |k: &str, d: i32| f64::from(cfg.get_int("Lag", k, d)) / 1000.0;
        Limits {
            ping_spec: int("PingToSpec", 600),
            ping_wpn_start: int("PingToStartIgnoringWeapons", 300),
            ping_wpn_all: int("PingToIgnoreAllWeapons", 1000),
            ping_noflags: int("PingToDisallowFlags", 500),
            s2c_spec: pct("S2CLossToSpec", 150),
            s2c_wpn_start: pct("S2CLossToStartIgnoringWeapons", 40),
            s2c_wpn_all: pct("S2CLossToIgnoreAllWeapons", 500),
            s2c_noflags: pct("S2CLossToDisallowFlags", 50),
            wpn_spec: pct("WeaponLossToSpec", 150),
            wpn_wpn_start: pct("WeaponLossToStartIgnoringWeapons", 40),
            wpn_wpn_all: pct("WeaponLossToIgnoreAllWeapons", 500),
            wpn_noflags: pct("WeaponLossToDisallowFlags", 50),
            c2s_spec: pct("C2SLossToSpec", 150),
            c2s_noflags: pct("C2SLossToDisallowFlags", 50),
            spike_spec: int("SpikeToSpec", 3000),
        }
    }
}

/// What lagaction decides for one player (ASSS `check_lag`).
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// Why the player must be in spectator mode (the first reason is the one
    /// they are told); empty if they may play.
    pub spec: Vec<String>,
    /// Barred from flags and balls.
    pub no_flags_balls: bool,
    /// Share of their weapons to ignore, 0 to 1.
    pub ignore_weapons: f64,
}

/// ASSS `check_lag` on a player's numbers.
pub fn judge(d: &LagData, l: &Limits) -> Verdict {
    let avg = d.weighted_ping();
    let (s2c, wpn, c2s) = (d.s2c_loss(), d.wpn_loss(), d.c2s_loss());
    let mut spec = Vec::new();
    if avg > l.ping_spec {
        spec.push(format!(
            "You have been specced for excessive ping ({avg} > {})",
            l.ping_spec
        ));
    }
    if s2c > l.s2c_spec {
        spec.push(format!(
            "You have been specced for excessive S2C packetloss ({:.2} > {:.2})",
            100.0 * s2c,
            100.0 * l.s2c_spec
        ));
    }
    if wpn > l.wpn_spec {
        spec.push(format!(
            "You have been specced for excessive S2C weapon packetloss ({:.2} > {:.2})",
            100.0 * wpn,
            100.0 * l.wpn_spec
        ));
    }
    if c2s > l.c2s_spec {
        spec.push(format!(
            "You have been specced for excessive C2S packetloss ({:.2} > {:.2})",
            100.0 * c2s,
            100.0 * l.c2s_spec
        ));
    }
    let no_flags_balls =
        avg > l.ping_noflags || s2c > l.s2c_noflags || wpn > l.wpn_noflags || c2s > l.c2s_noflags;
    // the worst of the three, as a share between "start" and "all"
    let ign1 = f64::from(avg - l.ping_wpn_start) / f64::from(l.ping_wpn_all - l.ping_wpn_start);
    let ign2 = (s2c - l.s2c_wpn_start) / (l.s2c_wpn_all - l.s2c_wpn_start);
    let ign3 = (wpn - l.wpn_wpn_start) / (l.wpn_wpn_all - l.wpn_wpn_start);
    let mut ign = ign1;
    if ign2 > ign {
        ign = ign2;
    }
    if ign3 > ign {
        ign = ign3;
    }
    // a zero-width range (start == all) can give NaN, which ASSS turns into
    // an undefined integer; ignore nothing instead
    let ignore_weapons = if ign.is_nan() {
        0.0
    } else {
        ign.clamp(0.0, 1.0)
    };
    Verdict {
        spec,
        no_flags_balls,
        ignore_weapons,
    }
}

fn spec_player(zone: &mut Zone, pid: PlayerId, a: crate::arena::ArenaId, why: &str) {
    let in_ship = zone.players.get(pid).is_some_and(|p| !p.ship.is_spec());
    if in_ship {
        let sf = zone.arenas.get(a).map_or(8025, super::arenaman::spec_freq);
        super::game::set_ship_and_freq(zone, pid, Ship::SPEC, sf);
        tracing::info!(target: "lagaction", pid = pid.0, "{why}");
        message(zone, pid, why);
    }
}

fn check(zone: &mut Zone) -> bool {
    let now = zone.now();
    for pid in zone.players.ids() {
        let Some(p) = zone.players.get(pid) else {
            continue;
        };
        let Some(a) = p.arena else { continue };
        if p.status != crate::player::PlayerStatus::Playing
            || !matches!(p.kind, ClientKind::Vie | ClientKind::Continuum)
        {
            continue;
        }
        let last_packet = p.last_packet;
        let Some(cfg) = zone.arenas.get(a).map(|x| x.cfg.clone()) else {
            continue;
        };
        let l = Limits::load(&cfg);
        // ASSS check_spike: time since anything arrived from the player
        if let Some(t) = last_packet {
            let spike = i64::try_from(now.since(t))
                .unwrap_or(i64::MAX)
                .saturating_mul(10);
            if spike > i64::from(l.spike_spec) {
                spec_player(
                    zone,
                    pid,
                    a,
                    &format!("You have been specced for a {spike}ms spike"),
                );
            }
        }
        let v = judge(&lag_data(zone, pid), &l);
        if let Some(first) = v.spec.first() {
            spec_player(zone, pid, a, first);
        }
        if let Some(x) = data(zone, pid) {
            x.no_ship = !v.spec.is_empty();
            x.no_flags_balls = v.no_flags_balls;
            x.ignore_weapons = v.ignore_weapons;
        }
        super::game::set_ignore_weapons(zone, pid, v.ignore_weapons);
    }
    true
}

fn c_lag(zone: &mut Zone, inv: &Invocation<'_>) {
    let t = match inv.target {
        Target::Player(t) => t,
        _ => inv.sender,
    };
    let name = zone
        .players
        .get(t)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let d = lag_data(zone, t);
    let lines = [
        format!(
            "{name}: avg ping: {}  ploss: s2c: {:.2} c2s: {:.2} s2cwpn: {:.2}",
            d.weighted_ping(),
            100.0 * d.s2c_loss(),
            100.0 * d.c2s_loss(),
            100.0 * d.wpn_loss()
        ),
        format!(
            "{name}: s2c ping: {} {} ({}-{})  c2s ping: {} {} ({}-{})",
            d.cping.current,
            d.cping.avg,
            d.cping.min.min(d.cping.max),
            d.cping.max,
            d.pping.current,
            d.pping.avg,
            d.pping.min.min(d.pping.max),
            d.pping.max
        ),
        format!(
            "{name}: rel ping: {} {} ({}-{})  packets: s {}/{}  c {}/{}",
            d.rping.current,
            d.rping.avg,
            d.rping.min.min(d.rping.max),
            d.rping.max,
            d.s_sent,
            d.s_received,
            d.c_sent,
            d.c_received
        ),
    ];
    for l in lines {
        message(zone, inv.sender, &l);
    }
    if d.ignore_weapons > 0.0 {
        let pct = (100.0 * d.ignore_weapons).round();
        message(
            zone,
            inv.sender,
            &format!("{name}: ignoring {pct}% of weapons"),
        );
    }
    if d.no_ship {
        message(zone, inv.sender, &format!("{name}: lag too high to play"));
    }
    if d.no_flags_balls {
        message(
            zone,
            inv.sender,
            &format!("{name}: lag too high to carry flags or balls"),
        );
    }
}

/// The `lagdata` module (`?lag`).
pub struct LagDataModule;

impl Module for LagDataModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.add_command(
            me,
            "lag",
            None,
            Some("Module: lagdata\nTargets: player or none\nArgs: none\nShows ping and packet loss.\n"),
            c_lag,
        );
        Ok(())
    }
}

/// The `lagaction` module.
pub struct LagActionModule;

impl Module for LagActionModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let every = u64::try_from(zone.cfg.global().get_int("Lag", "CheckInterval", 300))
            .unwrap_or(300)
            .max(1);
        zone.set_timer(me, every, every, check);
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        // don't leave players restricted
        for pid in zone.players.ids() {
            if let Some(d) = zone
                .players
                .get_mut(pid)
                .and_then(|x| x.ext.get_mut::<LagData>())
            {
                d.no_flags_balls = false;
                d.no_ship = false;
                d.ignore_weapons = 0.0;
            }
            super::game::set_ignore_weapons(zone, pid, 0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits::load(&crate::config::ConfigFile::default())
    }

    fn ping(avg: i32) -> LagData {
        let mut d = LagData::default();
        for s in [&mut d.pping, &mut d.cping, &mut d.rping] {
            s.avg = avg;
        }
        d
    }

    #[test]
    fn tiers() {
        let l = limits();
        // ASSS defaults: ignore from 300 ms (0%) to 1000 ms (100%)
        let v = judge(&ping(650), &l);
        assert_eq!(v.spec.len(), 1);
        assert!(v.no_flags_balls);
        assert!((v.ignore_weapons - 0.5).abs() < 1e-9);
        let v = judge(&ping(200), &l);
        assert!(v.spec.is_empty() && !v.no_flags_balls);
        assert_eq!(v.ignore_weapons, 0.0);
        // weapon loss: 30% of 1000 weapon packets lost
        let mut d = ping(0);
        d.wpn_sent = 1000;
        d.wpn_received = 700;
        let v = judge(&d, &l);
        assert_eq!(v.spec.len(), 1);
        assert!(v.spec[0].contains("weapon packetloss (30.00 > 15.00)"));
        assert!((v.ignore_weapons - (0.3 - 0.04) / (0.5 - 0.04)).abs() < 1e-9);
        // the worst of the three wins
        d.wpn_received = 1000;
        d.s_sent = 1000;
        d.c_received = 600;
        let want = (0.4 - 0.04) / (0.5 - 0.04);
        assert!((judge(&d, &l).ignore_weapons - want).abs() < 1e-9);
        // a zero-width range ignores nothing rather than misbehaving
        let flat = Limits {
            ping_wpn_start: 300,
            ping_wpn_all: 300,
            ..l
        };
        assert_eq!(judge(&ping(300), &flat).ignore_weapons, 0.0);
    }

    #[test]
    fn huge_client_counts_count_as_loss() {
        // ASSS would read these as negative ints and report no loss
        assert!(ploss(u64::from(u32::MAX), 1000) > 0.99);
        assert_eq!(ploss(150, 0), 0.0);
    }

    #[test]
    fn ping_average() {
        let mut p = PingStat::default();
        for s in [100, 100, 900] {
            p.add(s);
        }
        assert_eq!((p.current, p.min, p.max), (900, 100, 900));
        assert_eq!(p.avg, ((12 * 7 + 100) / 8 * 7 + 900) / 8);
    }
}
