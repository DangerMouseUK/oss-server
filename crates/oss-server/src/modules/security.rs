//! `security`: prize/door seed synchronisation and client checksum checks
//! (ASSS's `security:security`, which ASSS shipped only as a binary; this
//! follows the GPL reimplementation in Subspace Server .NET).
//!
//! * On arena entry a player gets S2C `0x18` with the current green and door
//!   seeds and the server time they apply from (key 0: no checks). Clients
//!   generate prizes and door states from these, so everyone in an arena sees
//!   the same greens and doors.
//! * Every minute (first after 25 s) new seeds and a random checksum key are
//!   drawn and sent to every playing client that has sent a position. The
//!   client answers with C2S `0x1A`: settings checksum, map checksum, exe
//!   checksum and lag statistics. Mismatches and missing answers (after 15 s)
//!   are logged and, with `Security:SecurityKickoff`, kicked (capability
//!   `bypasssecurity` exempts, `suppresssecurity` silences the log).
//! * The VIE exe checksum is computed; the Continuum one comes from the zone's
//!   `scrty` file (key/checksum pairs) when present, otherwise it isn't
//!   checked. A private build can supply its own by shipping that file.

use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s::{self, SecurityResponse};
use oss_proto::s2c::Security;

use crate::arena::{ArenaId, ArenaStatus};
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId, PlayerStatus};
use crate::prng::Prng;
use crate::registry::{Event, Owner, Scope};
use crate::zone::Zone;

use super::capman::Capabilities;
use super::clientset::ClientSettings;
use super::core::{Action, PlayerAction};

/// Exempts a player from security kicks.
pub const CAP_BYPASS_SECURITY: &str = "bypasssecurity";
/// Silences security log messages about a player.
pub const CAP_SUPPRESS_SECURITY: &str = "suppresssecurity";

/// Ticks before the first check.
const FIRST_CHECK: u64 = 2500;
/// Ticks between checks.
const INTERVAL: u64 = 6000;
/// Ticks a client has to answer.
const ANSWER_TIME: u64 = 1500;

/// New seeds were drawn (ASSS/.NET `SecuritySeedChanged`).
#[derive(Debug, Clone, Copy)]
pub struct SeedsChanged {
    /// Prize seed.
    pub green_seed: u32,
    /// Door seed.
    pub door_seed: u32,
    /// Server time they apply from.
    pub timestamp: u32,
}

impl Event for SeedsChanged {}

struct State {
    packet: Security,
    scrty: Option<Vec<u32>>,
    cont_exe: Option<u32>,
    vie_exe: u32,
    kickoff: bool,
}

#[derive(Default)]
struct ArenaSec {
    map_checksum: Option<u32>,
    map_crc: Option<u32>,
    override_seeds: Option<Security>,
}

#[derive(Default)]
struct PlayerSec {
    sent: bool,
    cancelled: bool,
    settings_checksum: Option<u32>,
}

/// Seed access for other modules (ASSS/.NET `ISecuritySeedSync`).
pub trait SeedSync: Send + Sync {
    /// The current `(green seed, door seed, timestamp)`.
    fn current_seeds(&self, zone: &Zone) -> Option<(u32, u32, u32)>;
    /// Fixes the seeds for one arena (e.g. replays), sending them now.
    fn override_arena_seeds(
        &self,
        zone: &mut Zone,
        arena: ArenaId,
        green: u32,
        door: u32,
        timestamp: u32,
    );
    /// Returns an arena to the shared seeds.
    fn remove_arena_override(&self, zone: &mut Zone, arena: ArenaId) -> bool;
}

fn encode(p: &Security) -> Vec<u8> {
    let mut b = Vec::with_capacity(17);
    p.encode(&mut b);
    b
}

fn has_cap(zone: &mut Zone, p: PlayerId, cap: &str) -> bool {
    zone.iface::<dyn Capabilities>()
        .is_some_and(|c| c.has_capability(zone, p, cap))
}

fn malicious(zone: &mut Zone, p: PlayerId, what: &str) {
    if !has_cap(zone, p, CAP_SUPPRESS_SECURITY) {
        tracing::warn!(target: "security", pid = p.0, "{what}");
    }
}

fn kick(zone: &mut Zone, p: PlayerId) {
    let kickoff = zone.res.get::<State>().is_some_and(|s| s.kickoff);
    if kickoff && !has_cap(zone, p, CAP_BYPASS_SECURITY) {
        tracing::info!(target: "security", pid = p.0, "kicking off for security violation");
        super::core::kick(zone, p);
    }
}

/// Draws new seeds and a key, and recomputes per-arena map checksums.
fn switch_checksums(zone: &mut Zone) {
    let now = zone.now().wire();
    let rng = zone.res.get_or_default::<Prng>();
    let (green, door, raw_key, pick) = (
        rng.next_u32(),
        rng.next_u32(),
        rng.next_u32(),
        rng.next_u32(),
    );
    let Some(st) = zone.res.get_mut::<State>() else {
        return;
    };
    st.packet.green_seed = green;
    st.packet.door_seed = door;
    st.packet.timestamp = now;
    match st.scrty.as_ref().filter(|s| s.len() >= 4) {
        Some(s) => {
            let pairs = (s.len() / 2) as u32;
            let i = (1 + pick % (pairs - 1)) as usize * 2;
            st.packet.key = s[i];
            st.cont_exe = s.get(i + 1).copied();
        }
        None => {
            st.packet.key = raw_key;
            st.cont_exe = None;
        }
    }
    st.vie_exe = oss_proto::checksum::vie_exe_checksum(st.packet.key);
    let key = st.packet.key;
    for id in zone.arenas.ids() {
        let running = zone
            .arenas
            .get(id)
            .is_some_and(|a| a.status == ArenaStatus::Running);
        let sums = if running {
            super::mapdata::arena_map(zone, id).map(|m| (m.checksum(key), m.crc))
        } else {
            None
        };
        if let Some(a) = zone.arenas.get_mut(id) {
            let d = a.ext.get_or_default::<ArenaSec>();
            d.map_checksum = sums.map(|s| s.0);
            d.map_crc = sums.map(|s| s.1);
        }
    }
    zone.fire(&SeedsChanged {
        green_seed: green,
        door_seed: door,
        timestamp: now,
    });
}

fn send_checks(zone: &mut Zone) -> bool {
    switch_checksums(zone);
    let Some(packet) = zone.res.get::<State>().map(|s| s.packet.clone()) else {
        return true;
    };
    let cs = zone.iface::<dyn ClientSettings>();
    let mut to = Vec::new();
    for pid in zone.players.ids() {
        let Some(p) = zone.players.get(pid) else {
            continue;
        };
        let Some(arena) = p.arena else { continue };
        let overridden = zone
            .arenas
            .get(arena)
            .and_then(|a| a.ext.get::<ArenaSec>())
            .is_some_and(|d| d.override_seeds.is_some());
        if overridden {
            continue;
        }
        let eligible = p.status == PlayerStatus::Playing
            && matches!(p.kind, ClientKind::Vie | ClientKind::Continuum)
            && super::game::has_sent_position(zone, pid);
        let sum = if eligible {
            cs.as_ref().and_then(|c| c.checksum(zone, pid, packet.key))
        } else {
            None
        };
        if let Some(p) = zone.players.get_mut(pid) {
            let d = p.ext.get_or_default::<PlayerSec>();
            d.sent = eligible;
            d.cancelled = false;
            d.settings_checksum = sum;
        }
        if eligible {
            to.push(pid);
        }
    }
    let n = to.len();
    zone.send_many(to, &encode(&packet), SendFlags::RELIABLE);
    tracing::debug!(target: "security", players = n, green = packet.green_seed, door = packet.door_seed, "sent security check");
    true
}

fn check_answers(zone: &mut Zone) -> bool {
    for pid in zone.players.ids() {
        let sent = zone
            .players
            .get(pid)
            .and_then(|p| p.ext.get::<PlayerSec>())
            .is_some_and(|d| d.sent);
        if sent {
            malicious(zone, pid, "no security packet response");
            kick(zone, pid);
        }
    }
    false
}

fn on_response(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (kind, arena) = (p.kind, p.arena);
    let want = if kind == ClientKind::Continuum {
        SecurityResponse::CONT_LEN
    } else {
        SecurityResponse::VIE_LEN
    };
    if pkt.len() != want {
        malicious(
            zone,
            pid,
            &format!("invalid security response length {}", pkt.len()),
        );
        return;
    }
    let Some(arena) = arena else {
        malicious(zone, pid, "security response outside an arena");
        return;
    };
    let Ok(r) = SecurityResponse::decode(pkt) else {
        return;
    };
    let (sent, cancelled, settings) = zone
        .players
        .get(pid)
        .and_then(|p| p.ext.get::<PlayerSec>())
        .map_or((false, false, None), |d| {
            (d.sent, d.cancelled, d.settings_checksum)
        });
    if let Some(d) = zone
        .players
        .get_mut(pid)
        .map(|p| p.ext.get_or_default::<PlayerSec>())
    {
        d.sent = false;
        d.cancelled = false;
    }
    if !sent {
        if !cancelled {
            malicious(zone, pid, "unexpected security response");
        }
        return;
    }
    let (map_sum, map_crc) = zone
        .arenas
        .get(arena)
        .and_then(|a| a.ext.get::<ArenaSec>())
        .map_or((None, None), |d| (d.map_checksum, d.map_crc));
    let (vie_exe, cont_exe) = zone
        .res
        .get::<State>()
        .map_or((0, None), |s| (s.vie_exe, s.cont_exe));
    let mut bad = false;
    if settings.is_some_and(|s| s != r.settings_checksum) {
        malicious(zone, pid, "settings checksum mismatch");
        bad = true;
    }
    if map_sum.is_some_and(|s| s != r.map_checksum) {
        malicious(zone, pid, "map checksum mismatch");
        bad = true;
    }
    let exe_ok = match kind {
        ClientKind::Vie => r.exe_checksum == vie_exe,
        ClientKind::Continuum => {
            if let (Some(crc), Some((_, client_crc))) = (map_crc, r.continuum)
                && crc != client_crc
            {
                malicious(zone, pid, "map CRC-32 mismatch");
                bad = true;
            }
            cont_exe.is_none_or(|c| c == r.exe_checksum)
        }
        _ => true,
    };
    if !exe_ok {
        malicious(zone, pid, "exe checksum mismatch");
        bad = true;
    }
    // the client reports pings in ticks
    super::lagdata::client_ping(zone, pid, i32::from(r.ping_average) * 10);
    tracing::debug!(target: "security", pid = pid.0, ok = !bad, ping = r.ping_average, "security response");
    if bad {
        kick(zone, pid);
    }
}

fn on_violation(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    match pkt.get(1) {
        Some(v) => malicious(
            zone,
            pid,
            &format!("client reported security violation {v:#04x}"),
        ),
        None => malicious(zone, pid, "invalid security violation packet"),
    }
}

/// The seed-sync implementation.
pub struct SecurityImpl;

impl SeedSync for SecurityImpl {
    fn current_seeds(&self, zone: &Zone) -> Option<(u32, u32, u32)> {
        zone.res
            .get::<State>()
            .map(|s| (s.packet.green_seed, s.packet.door_seed, s.packet.timestamp))
    }

    fn override_arena_seeds(
        &self,
        zone: &mut Zone,
        arena: ArenaId,
        green: u32,
        door: u32,
        timestamp: u32,
    ) {
        let p = Security {
            green_seed: green,
            door_seed: door,
            timestamp,
            key: 0,
        };
        let Some(a) = zone.arenas.get_mut(arena) else {
            return;
        };
        a.ext.get_or_default::<ArenaSec>().override_seeds = Some(p.clone());
        zone.send_to_arena(arena, None, &encode(&p), SendFlags::RELIABLE);
    }

    fn remove_arena_override(&self, zone: &mut Zone, arena: ArenaId) -> bool {
        let Some(a) = zone.arenas.get_mut(arena) else {
            return false;
        };
        if a.ext
            .get_or_default::<ArenaSec>()
            .override_seeds
            .take()
            .is_none()
        {
            return false;
        }
        let Some(mut p) = zone.res.get::<State>().map(|s| s.packet.clone()) else {
            return true;
        };
        p.key = 0;
        zone.send_to_arena(arena, None, &encode(&p), SendFlags::RELIABLE);
        true
    }
}

/// The `security` module.
pub struct SecurityModule;

impl Module for SecurityModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let scrty = zone
            .cfg
            .source()
            .read(std::path::Path::new("scrty"))
            .map(|b| {
                b.as_chunks::<4>()
                    .0
                    .iter()
                    .take(1000)
                    .map(|w| u32::from_le_bytes(*w))
                    .collect::<Vec<u32>>()
            });
        let kickoff = zone.cfg.global().get_int("Security", "SecurityKickoff", 0) != 0;
        zone.res.insert(State {
            packet: Security {
                green_seed: 0,
                door_seed: 0,
                timestamp: 0,
                key: 0,
            },
            scrty,
            cont_exe: None,
            vie_exe: 0,
            kickoff,
        });
        switch_checksums(zone);
        zone.reg
            .provide::<dyn SeedSync>(me, Scope::Global, Arc::new(SecurityImpl));
        zone.on_packet(me, c2s::kind::SECURITY_RESPONSE, on_response);
        zone.on_packet(me, c2s::kind::SECURITY_VIOLATION, on_violation);
        zone.set_timer(me, FIRST_CHECK, INTERVAL, move |z| {
            send_checks(z);
            z.set_timer(me, ANSWER_TIME, 0, check_answers);
            true
        });
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| match ev.action {
            Action::EnterArena => {
                let Some(arena) = ev.arena else { return };
                let over = z
                    .arenas
                    .get(arena)
                    .and_then(|a| a.ext.get::<ArenaSec>())
                    .and_then(|d| d.override_seeds.clone());
                let Some(mut p) = over.or_else(|| z.res.get::<State>().map(|s| s.packet.clone()))
                else {
                    return;
                };
                p.key = 0;
                z.send(ev.player, &encode(&p), SendFlags::RELIABLE);
            }
            Action::LeaveArena => {
                if let Some(d) = z
                    .players
                    .get_mut(ev.player)
                    .map(|p| p.ext.get_or_default::<PlayerSec>())
                    && d.sent
                {
                    d.sent = false;
                    d.cancelled = true;
                }
            }
            _ => {}
        });
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        zone.res.remove::<State>();
    }
}
