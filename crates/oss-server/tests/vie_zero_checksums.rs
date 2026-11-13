//! A plain VIE 1.34 client can log in, enter an arena and survive the periodic
//! security check on a stock build.
//!
//! In the VIE protocol zero means "not applicable", and it means it in both
//! directions:
//!
//! * the login response's `codechecksum` -- the original `subgame` sends 0,
//!   and a genuine client answers ASSS's computed `0x281C_C948` with
//!   `SEC_LoginChecksumMismatch` (`0x17`) and exits;
//! * the security response's `exe_checksum` -- a genuine client never computes
//!   one and always reports 0, measured off the 1998 binary on 2026-11-13 over
//!   four samples, four keys and three sessions.
//!
//! This zone therefore runs with `Security:SecurityKickoff = 1`. It defaults
//! to 0, which is the only reason the second of those was a log line rather
//! than an ejection, so a test that left it off would pass for the wrong
//! reason. The player here holds no capabilities: not `bypasssecurity`, and
//! not `seeprivfreq`, which would mark it staff and have the server send
//! `0xFFFF_FFFF` for both checksums -- the one case that always worked, and
//! the one that hid all of this.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login, SecurityResponse};
use oss_proto::position::C2SPosition;
use oss_proto::s2c;
use oss_proto::wire::FixedStr;

use oss_server::clientset::SettingsBlock;
use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::modules::mapdata::ArenaMap;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

/// The periodic check goes out at tick 2500 and every 6000 after it.
const INTERVAL: u64 = 6000;
/// Ticks a client has to answer before it is kicked for silence.
const ANSWER_TIME: u64 = 1500;

/// A map with a border and a blob, so the map checksum the client has to
/// return is something other than the empty grid's.
fn synthetic_map() -> Vec<u8> {
    let mut out = Vec::new();
    let mut put =
        |x: u32, y: u32, t: u32| out.extend_from_slice(&(x | (y << 12) | (t << 24)).to_le_bytes());
    for i in 0..1024 {
        put(i, 0, 1);
        put(i, 1023, 1);
        put(0, i, 1);
        put(1023, i, 1);
    }
    for i in 0..20 {
        put(500 + i, 500, 2);
    }
    out
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new() -> Self {
        let mut src = MemSource::new([
            ("conf/global.conf", "[Security]\nSecurityKickoff = 1\n"),
            ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
            // no capabilities at all: an ordinary player, not staff
            ("conf/groupdef.conf", "[default]\n"),
            ("conf/staff.conf", ""),
            (
                "arenas/(default)/arena.conf",
                "[General]\nMap = test.lvl\n[Team]\nDesiredTeams = 2\n",
            ),
        ]);
        src.insert("maps/test.lvl", synthetic_map());
        let zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
        }
    }

    fn tick(&mut self, n: u64) {
        for _ in 0..n {
            self.t += 1;
            self.zone.set_now(Ticks(self.t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
    }

    fn send(&mut self, p: PlayerId, data: &[u8]) {
        self.zone.handle_packet(p, data);
        self.lo.collect(&mut self.zone);
    }

    fn take(&mut self, p: PlayerId) -> Vec<Vec<u8>> {
        self.lo.take(p).into_iter().map(|(d, _)| d).collect()
    }

    fn gone(&self, p: PlayerId) -> bool {
        self.zone.players.get(p).is_none()
    }

    /// Logs a VIE player in and returns the login response it was sent.
    fn login(&mut self, name: &str) -> (PlayerId, s2c::LoginResponse) {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        let mut buf = Vec::new();
        Login {
            kind: c2s::kind::LOGIN,
            flags: 0,
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(b"x"),
            machine_id: 1,
            connect_type: 4,
            timezone_bias: 0,
            unknown1: 0,
            client_version: 134,
            field444: 444,
            bound_ip: 0,
            permission_id: 1,
            server_ip: 0,
            bound_port: 0,
            redirect_id: 0,
            using_proxy: 0,
            unused: 0,
            cont_id: None,
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        let lr = self
            .take(p)
            .iter()
            .find(|d| d.first() == Some(&s2c::kind::LOGIN_RESPONSE))
            .map(|d| s2c::LoginResponse::decode(d).expect("0x0A"))
            .expect("a login response");
        (p, lr)
    }

    /// Enters the arena, takes a ship and sends a position: without one the
    /// security module skips the player. Returns the settings packet, which
    /// the settings checksum is computed over.
    fn enter(&mut self, p: PlayerId) -> Vec<u8> {
        let mut buf = Vec::new();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            ..Default::default()
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        let pkts = self.take(p);
        assert!(
            pkts.iter()
                .any(|d| d.first() == Some(&s2c::kind::ENTERING_ARENA)),
            "the player must actually enter the arena"
        );
        let settings = pkts
            .into_iter()
            .find(|d| d.first() == Some(&s2c::kind::SETTINGS))
            .expect("settings packet");
        self.send(p, &[c2s::kind::SET_SHIP, 0]);
        buf.clear();
        C2SPosition {
            time: u32::try_from(self.t).expect("tick fits"),
            x: 512 * 16,
            y: 512 * 16,
            ..Default::default()
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(5);
        self.take(p);
        settings
    }

    /// Runs on to the next periodic check and returns its key. Stopping
    /// exactly on it matters: running past it runs past the answer deadline
    /// too.
    fn next_challenge(&mut self, p: PlayerId) -> u32 {
        let until = (2500 + INTERVAL - (self.t % INTERVAL)) % INTERVAL;
        self.tick(if until == 0 { INTERVAL } else { until });
        let sec = self
            .take(p)
            .iter()
            .filter(|d| d.first() == Some(&s2c::kind::SECURITY))
            .map(|d| s2c::Security::decode(d).expect("0x18"))
            .next_back()
            .expect("a periodic security check");
        assert_ne!(sec.key, 0, "the periodic check must carry a key");
        sec.key
    }
}

/// What a real VIE 1.34 client answers a check with: every field right for
/// `key`, and `exe_checksum` left at zero because it does not compute one.
fn vie_answer(settings: &[u8], key: u32) -> SecurityResponse {
    let block = SettingsBlock::from_bytes(settings.try_into().expect("1428 bytes"));
    SecurityResponse {
        weapon_count: 1,
        settings_checksum: block.checksum(key),
        exe_checksum: 0,
        map_checksum: ArenaMap::from_bytes(&synthetic_map()).checksum(key),
        s2c_slow_total: 0,
        s2c_fast_total: 0,
        s2c_slow_current: 0,
        s2c_fast_current: 0,
        s2c_reliable_out: 0,
        ping_last: 10,
        ping_average: 10,
        ping_low: 10,
        ping_high: 10,
        slow_frame: 0,
        continuum: None,
    }
}

#[test]
fn a_plain_vie_player_is_sent_a_zero_code_checksum() {
    let mut h = H::new();
    let (p, lr) = h.login("VieTest134");
    assert_eq!(lr.code, s2c::login_code::OK, "an ordinary player gets in");
    // Not staff: staff get 0xFFFFFFFF for both, which makes the client skip
    // the comparison entirely and is what masked this for so long.
    assert_eq!(
        lr.exe_checksum,
        core_mod::VIE_EXE_CHECKSUM,
        "the exe checksum is the CRC-32 of SUBSPACE.EXE and stays"
    );
    assert_eq!(lr.is_vip, 0, "this player is not VIP");
    // The whole point: subgame sends zero here, and a genuine 1.34 client
    // rejects anything else as a code checksum mismatch.
    assert_eq!(
        lr.code_checksum, 0,
        "zero means \"not applicable\"; a computed value is 0x17 and an exit"
    );
    h.enter(p);
    assert!(!h.gone(p), "and it is still in the arena");
}

#[test]
fn a_vie_client_that_reports_no_exe_checksum_survives_the_check() {
    let mut h = H::new();
    let (p, lr) = h.login("VieTest134");
    assert_eq!(lr.exe_checksum, core_mod::VIE_EXE_CHECKSUM, "not staff");
    let settings = h.enter(p);
    let key = h.next_challenge(p);
    // The value the server computes for this key is not itself zero, so
    // accepting the client's zero is a skip and not a coincidence.
    assert_ne!(
        oss_proto::checksum::vie_exe_checksum(key),
        0,
        "the expected checksum must be worth comparing"
    );
    let mut buf = Vec::new();
    vie_answer(&settings, key).encode(&mut buf);
    assert_eq!(buf.len(), SecurityResponse::VIE_LEN);
    h.send(p, &buf);
    h.tick(5);
    assert!(!h.gone(p), "an honest VIE answer is not a kick");
    // Past the answer deadline with room to spare, but short of the next
    // check, so nothing else can be the cause.
    h.tick(ANSWER_TIME + 500);
    assert!(
        !h.gone(p),
        "and it was not then kicked for having said nothing"
    );
}

#[test]
fn a_wrong_nonzero_exe_checksum_is_still_kicked() {
    // Guards the test above from passing for the wrong reason twice over:
    // `SecurityKickoff` really is on in this zone, and the VIE arm still
    // compares when the client offers something to compare.
    let mut h = H::new();
    let (p, _) = h.login("VieTest134");
    let settings = h.enter(p);
    let key = h.next_challenge(p);
    let mut r = vie_answer(&settings, key);
    r.exe_checksum = oss_proto::checksum::vie_exe_checksum(key) ^ 1;
    let mut buf = Vec::new();
    r.encode(&mut buf);
    h.send(p, &buf);
    h.tick(5);
    assert!(h.gone(p), "a wrong checksum is still a kick");
}

#[test]
fn a_vie_client_that_computes_the_checksum_is_still_accepted() {
    // MervBot and this project's own client do compute it, and must not be
    // broken by making zero acceptable.
    let mut h = H::new();
    let (p, _) = h.login("VieTest134");
    let settings = h.enter(p);
    let key = h.next_challenge(p);
    let mut r = vie_answer(&settings, key);
    r.exe_checksum = oss_proto::checksum::vie_exe_checksum(key);
    let mut buf = Vec::new();
    r.encode(&mut buf);
    h.send(p, &buf);
    h.tick(ANSWER_TIME + 500);
    assert!(!h.gone(p), "a correct checksum is still accepted");
}
