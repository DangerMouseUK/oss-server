//! `security`: prize/door seed synchronisation and client checksum checks
//! (ASSS's `security:security`, which ASSS shipped only as a binary; this
//! follows the GPL reimplementation in Subspace Server .NET).
//!
//! * On arena entry a player gets S2C `0x18` with the current green and door
//!   seeds and the server time they apply from (key 0: no checks). Clients
//!   generate prizes and door states from these, so everyone in an arena sees
//!   the same greens and doors.
//! * Every minute (first after 25 s) new seeds and a checksum key are drawn
//!   and sent to every playing client that has sent a position. The key comes
//!   from the zone's key table, or, when it has none, from a draw bounded to
//!   the range a key may take -- see [`MAX_KEY`], which is where the one
//!   thing a server must not get wrong about this packet is written down. The
//!   client answers with C2S `0x1A`: settings checksum, map checksum, exe
//!   checksum and lag statistics. Mismatches and missing answers (after 15 s)
//!   are logged and, with `Security:SecurityKickoff`, kicked (capability
//!   `bypasssecurity` exempts, `suppresssecurity` silences the log).
//! * The Continuum exe checksum comes from the zone's `scrty` file
//!   (key/checksum pairs) when present, otherwise it isn't checked; a private
//!   build can supply its own by shipping that file. The VIE one is computed,
//!   but a client that reports zero -- which a genuine VIE 1.34 client always
//!   does -- is not held to it. See `check_exe` below for why.

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

/// The largest value a security-check key may take.
///
/// Public because it is a fact about the protocol rather than about this
/// module: anything that chooses a key, here or in another implementation
/// linking this crate, is bound by it.
///
/// **A checksum key is a 31-bit number, not a `u32`.** Every key table this
/// protocol has ever been given -- three distinct files written by three
/// different generators, 1,500 keys counted rather than sampled -- draws from
/// `1..=0x7FFF_FFFF` and never sets bit 31. The other 31 bits are evenly
/// spread, so the bound is the range itself and not an artefact of one
/// generator.
///
/// Two things go wrong above it, and the second is fatal.
///
/// 1. **No two implementations agree what the answer is.** The map checksum
///    starts its walk at row `key % 32` and column `key % 31`. ASSS, Subspace
///    Server .NET (`IMapData.GetChecksum`, "the same way asss 1.4.4
///    calculates the checksum") and nullspace (`Map::GetChecksum`) all take
///    those two remainders on a **signed** copy of the key;
///    [`oss_proto::checksum::map_checksum`] takes them unsigned. Below
///    `0x8000_0000` the two readings are the same arithmetic and agree
///    exactly. At or above it the signed reading starts at a negative row and
///    column and the unsigned one does not, so the expected answer depends on
///    which reading a given client was built from. **A server that sends such
///    a key is asking a question it cannot score.**
/// 2. **A genuine SubSpace 1.34 client does not survive one.** On 2026-11-14
///    the 1998 client answered a key from this range correctly and then, on
///    the next check sixty seconds later, received a key with bit 31 set and
///    stopped dead -- no response, and no position packets either, until the
///    session timed out. Both keys were recovered afterwards from the seeds
///    the run logs record, so this is the measured pair and not an inference
///    about which key it was. Silence on every channel at once is the client
///    ceasing to run, not disagreeing about a checksum: a disagreement would
///    have arrived as a `0x1A` we scored `ok=false`. Of the three numbers a
///    key is used to compute, two consume it purely as data; only the map
///    walk turns it into a position, and only above this bound.
///
/// Zero is excluded at the other end for a protocol reason rather than a
/// safety one: key 0 is the defined "these are seeds only, do not answer"
/// value, which is why [`SecurityModule`] sends it on arena entry and why
/// ASSS, Subspace Server .NET and this module all skip a table's first pair.
pub const MAX_KEY: u32 = 0x7FFF_FFFF;

/// The checksum key a zone with no key table sends, drawn from `raw`.
///
/// The whole of the choice is [`MAX_KEY`]: keep the draw, put it in the range
/// a key actually occupies. This is **not** a mask that papers over a client
/// bug -- it is the key space the protocol has, and `Prng::next_u32` was
/// simply handing out numbers from outside it. Zones with a key table are
/// unaffected; they were already drawing from this range, which is why they
/// never showed the defect.
fn fallback_key(raw: u32) -> u32 {
    match raw & MAX_KEY {
        // One draw in 2^31. A zero here would mean "do not answer", and
        // `send_checks` would then kick every playing client 15 s later for
        // not answering something it was told not to answer.
        0 => 1,
        k => k,
    }
}

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
        // No table: draw one. See `fallback_key` -- the range is the point.
        None => {
            st.packet.key = fallback_key(raw_key);
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

/// Whether a VIE client's reported exe checksum is acceptable.
///
/// **Zero is accepted on purpose; do not tighten this to a plain equality.**
/// A genuine VIE 1.34 client never computes this field and always leaves it
/// at zero, so here zero really does mean "not applicable".
///
/// **This is not a general rule about zero, and the login response is not an
/// instance of it.** This doc comment used to say the two directions followed
/// the same convention, and [`core::VIE_CODE_CHECKSUM`](super::core::VIE_CODE_CHECKSUM)
/// was set to zero partly on the strength of that symmetry. It is false: in
/// the login response the client *computes* the value and compares it, so a
/// zero there is a mismatch and an immediate disconnect. The two fields hold
/// the same kind of number and are read by opposite ends for opposite
/// purposes; **a convention one end observes says nothing about the other.**
///
/// Measured 2026-11-13 off the 1998 client in an arena -- four samples, four
/// keys, three sessions, with the settings and map checksums in the same
/// packets correct every time. So `reported == expected` alone is
/// unsatisfiable for a real client, and with `Security:SecurityKickoff` on
/// (it defaults to off, which is the only reason this was a log line rather
/// than an ejection) the zone ejects every honest VIE player within a minute.
///
/// [`vie_exe_checksum`](oss_proto::checksum::vie_exe_checksum) is not the
/// problem: all 36 of its constants are words read out of `SUBSPACE.EXE` 1.34
/// at a uniform `0x1800` stride, and the login-side `0xF142_9CE8` is that
/// binary's CRC-32. The routine is genuinely VIE's; asking a VIE client for
/// it is the bug. Clients that do compute it -- MervBot, and this project's
/// own -- are still held to it, which is as much as the check can be worth
/// once the one client that matters declines to answer. This mirrors the
/// Continuum arm, which likewise skips when there is nothing to compare.
fn check_exe(reported: u32, expected: u32) -> bool {
    reported == 0 || reported == expected
}

fn on_response(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (kind, arena) = (p.kind, p.arena);
    let (sent, cancelled, settings) = zone
        .players
        .get(pid)
        .and_then(|p| p.ext.get::<PlayerSec>())
        .map_or((false, false, None), |d| {
            (d.sent, d.cancelled, d.settings_checksum)
        });
    // Cleared before any early return below: a malformed answer is still an
    // answer, and leaving the flag set makes `check_answers` kick the player
    // 15 s later for "no security packet response", naming the wrong cause.
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
    // Continuum 0.38 answers with the timer drift and stops
    // ([`SecurityResponse::CONT_LEN`]); later builds append a map CRC-32
    // ([`SecurityResponse::CONT_CRC_LEN`]). Both are genuine.
    let len_ok = if kind == ClientKind::Continuum {
        SecurityResponse::is_continuum_len(pkt.len())
    } else {
        pkt.len() == SecurityResponse::VIE_LEN
    };
    if !len_ok {
        malicious(
            zone,
            pid,
            &format!("invalid security response length {}", pkt.len()),
        );
        kick(zone, pid);
        return;
    }
    let Some(arena) = arena else {
        malicious(zone, pid, "security response outside an arena");
        return;
    };
    let Ok(r) = SecurityResponse::decode(pkt) else {
        return;
    };
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
        ClientKind::Vie => check_exe(r.exe_checksum, vie_exe),
        ClientKind::Continuum => {
            if let (Some(crc), Some((_, Some(client_crc)))) = (map_crc, r.continuum)
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
    super::lagdata::client_ping(zone, pid, i32::from(r.ping_average) * 10, r.weapon_count);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prng::Prng;

    /// A map with something on every tile, so two samplings of it that differ
    /// at all differ in their sum.
    fn tile(x: i32, y: i32) -> u8 {
        if (0..1024).contains(&x) && (0..1024).contains(&y) {
            ((x.wrapping_mul(31).wrapping_add(y.wrapping_mul(17))).rem_euclid(160) + 1) as u8
        } else {
            0
        }
    }

    /// The map checksum as ASSS, Subspace Server .NET and nullspace all read
    /// it: the two remainders taken on a **signed** copy of the key.
    ///
    /// Transcribed from Subspace Server .NET `Modules/MapData.cs`
    /// (`int saveKey = (int)key; for (short y = (short)(saveKey % 32); ...)`)
    /// and nullspace `src/null/Map.cpp` (`int basekey = key; for (int y =
    /// basekey % 32; ...)`), which agree with each other. It is deliberately
    /// **not** derived from anything this module believes, so that the test
    /// below can disagree with us.
    fn signed_start_reading(key: u32) -> u32 {
        let base = key as i32;
        let mut sum = key;
        let mut y = base % 32;
        while y < 1024 {
            let mut x = base % 31;
            while x < 1024 {
                let t = tile(x, y);
                if oss_proto::checksum::counts_for_map_checksum(t) {
                    sum = sum.wrapping_add(key ^ u32::from(t));
                }
                x += 31;
            }
            y += 32;
        }
        sum
    }

    fn ours(key: u32) -> u32 {
        oss_proto::checksum::map_checksum(key, tile)
    }

    /// The guard is not vacuous: the two readings really do part company, and
    /// they part company exactly at [`MAX_KEY`].
    ///
    /// Without this, a `fallback_key` that returned a constant would pass the
    /// test below for the wrong reason.
    #[test]
    fn the_two_readings_agree_below_the_bound_and_not_above_it() {
        for k in [1u32, 2, 0x1234_5678, MAX_KEY - 1, MAX_KEY] {
            assert_eq!(ours(k), signed_start_reading(k), "agree at {k:#010x}");
        }
        for k in [0x8000_0000u32, 0x8000_0001, 0xDEAD_BEEF, 0xFFFF_FFFF] {
            assert_ne!(ours(k), signed_start_reading(k), "part at {k:#010x}");
        }
    }

    /// **The regression.** Every key a tableless zone can send must be one the
    /// readings agree on, and must not be the reserved zero.
    ///
    /// Against the behaviour this replaced -- `st.packet.key = raw_key`, the
    /// raw 32-bit draw -- this fails on the first draw with bit 31 set, which
    /// is roughly every other one. The assertion at the end is what keeps it
    /// honest: it fails if the draws never exercised the case at all.
    #[test]
    fn every_key_a_tableless_zone_sends_is_one_the_readings_agree_on() {
        let mut prng = Prng::new(0x5EED_1234_5EED_1234);
        let mut exercised = 0usize;
        for _ in 0..512 {
            let raw = prng.next_u32();
            exercised += usize::from(raw >= 0x8000_0000);
            let key = fallback_key(raw);
            assert_ne!(key, 0, "key 0 means 'do not answer'");
            assert_eq!(
                ours(key),
                signed_start_reading(key),
                "a key no two implementations score alike"
            );
        }
        assert!(
            exercised > 100,
            "only {exercised} draws had bit 31 set, so the case under test was barely reached"
        );
    }

    /// The bound keeps the draw and nothing else: it is a range, not a
    /// reshaping. Every key in range comes back untouched.
    #[test]
    fn a_key_already_in_range_is_passed_through() {
        for k in [1u32, 2, 0x4000_0000, MAX_KEY] {
            assert_eq!(fallback_key(k), k);
        }
        assert_eq!(fallback_key(0), 1, "the reserved value is nudged off");
        assert_eq!(fallback_key(0x8000_0000), 1, "and so is a masked zero");
    }
}
