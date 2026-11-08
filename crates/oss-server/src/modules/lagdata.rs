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
//! `lagaction` checks every `Lag:CheckInterval` ticks: players over the
//! `Lag:PingToSpec` / `*LossToSpec` limits are moved to spectator mode, and
//! those over the `*ToDisallowFlags` limits can't pick up flags or balls.
//! `?lag` shows a player's numbers.

use crate::commands::{Invocation, Target};
use crate::module::{Module, ModuleError};
use crate::player::{PlayerId, Ship};
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
    /// Too lagged to touch flags and balls.
    pub no_flags_balls: bool,
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

/// Position packet latency (one way, ms).
pub fn position(zone: &mut Zone, p: PlayerId, one_way_ms: i32) {
    if let Some(d) = data(zone, p) {
        d.pping.add(one_way_ms * 2);
    }
}

/// Client-reported average ping (ms).
pub fn client_ping(zone: &mut Zone, p: PlayerId, ms: i32) {
    if let Some(d) = data(zone, p) {
        d.cping.add(ms);
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

/// True if lagaction has barred the player from flags and balls.
pub fn no_flags_balls(zone: &Zone, p: PlayerId) -> bool {
    zone.players
        .get(p)
        .and_then(|x| x.ext.get::<LagData>())
        .is_some_and(|d| d.no_flags_balls)
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    ping_spec: i32,
    ping_noflags: i32,
    s2c_spec: f64,
    s2c_noflags: f64,
    c2s_spec: f64,
    c2s_noflags: f64,
}

fn limits(cfg: &crate::config::ConfigFile) -> Limits {
    // loss settings are in tenths of a percent
    let pct = |k: &str, d: i32| f64::from(cfg.get_int("Lag", k, d)) / 1000.0;
    Limits {
        ping_spec: cfg.get_int("Lag", "PingToSpec", 600),
        ping_noflags: cfg.get_int("Lag", "PingToDisallowFlags", 500),
        s2c_spec: pct("S2CLossToSpec", 150),
        s2c_noflags: pct("S2CLossToDisallowFlags", 50),
        c2s_spec: pct("C2SLossToSpec", 150),
        c2s_noflags: pct("C2SLossToDisallowFlags", 50),
    }
}

fn check(zone: &mut Zone) -> bool {
    for pid in zone.players.ids() {
        let Some(p) = zone.players.get(pid) else {
            continue;
        };
        let Some(a) = p.arena else { continue };
        if p.status != crate::player::PlayerStatus::Playing {
            continue;
        }
        let Some(cfg) = zone.arenas.get(a).map(|x| x.cfg.clone()) else {
            continue;
        };
        let l = limits(&cfg);
        let d = lag_data(zone, pid);
        let avg = d.weighted_ping();
        let (s2c, c2s) = (d.s2c_loss(), d.c2s_loss());
        let mut reasons = Vec::new();
        if avg > l.ping_spec {
            reasons.push(format!(
                "You have been specced for excessive ping ({avg} > {})",
                l.ping_spec
            ));
        }
        if s2c > l.s2c_spec {
            reasons.push(format!(
                "You have been specced for excessive S2C packetloss ({:.2} > {:.2})",
                100.0 * s2c,
                100.0 * l.s2c_spec
            ));
        }
        if c2s > l.c2s_spec {
            reasons.push(format!(
                "You have been specced for excessive C2S packetloss ({:.2} > {:.2})",
                100.0 * c2s,
                100.0 * l.c2s_spec
            ));
        }
        let in_ship = zone.players.get(pid).is_some_and(|p| !p.ship.is_spec());
        if in_ship && let Some(first) = reasons.first().cloned() {
            let sf = zone.arenas.get(a).map_or(8025, super::arenaman::spec_freq);
            super::game::set_ship_and_freq(zone, pid, Ship::SPEC, sf);
            tracing::info!(target: "lagaction", pid = pid.0, "{first}");
            message(zone, pid, &first);
        }
        let bar = avg > l.ping_noflags || s2c > l.s2c_noflags || c2s > l.c2s_noflags;
        if let Some(x) = data(zone, pid) {
            x.no_flags_balls = bar;
        }
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
            "{name}: avg ping: {}  ploss: s2c: {:.2} c2s: {:.2}",
            d.weighted_ping(),
            100.0 * d.s2c_loss(),
            100.0 * d.c2s_loss()
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
