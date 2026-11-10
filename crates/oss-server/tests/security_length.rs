//! The security response length gate, and what a Continuum client's missing
//! map CRC-32 means.
//!
//! Continuum 0.40 answers S2C `0x18` with the forty common bytes and a timer
//! drift and stops there — 42 bytes, read off its own `0x1A` builder. Later
//! builds append a map CRC-32, for 46. A server that insists on one of the two
//! rejects every client of the other kind, once a minute, for ever; that is
//! exactly what this server did before `SecurityResponse::is_continuum_len`,
//! and nothing here covered it.
//!
//! Driven in process over `Loopback`, because the Continuum forms are not
//! reachable over UDP: the public build's `VieAcceptor` is constructed with
//! `accept_continuum: false`.

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

/// A map with a border and a blob, so both the map checksum and the map
/// CRC-32 are something other than the empty grid's.
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

    /// Logs a player in, enters the arena, takes a ship and sends a position:
    /// without one the security module skips the player. Returns the settings
    /// packet, which the settings checksum is computed over.
    fn player(&mut self, kind: ClientKind) -> (PlayerId, Vec<u8>) {
        let p = core_mod::new_connection(&mut self.zone, kind, None).expect("slot");
        let mut buf = Vec::new();
        Login {
            kind: if kind == ClientKind::Continuum {
                c2s::kind::CONT_LOGIN
            } else {
                c2s::kind::LOGIN
            },
            flags: 0,
            name: FixedStr::new(b"tester"),
            password: FixedStr::new(b"x"),
            machine_id: 1,
            connect_type: 4,
            timezone_bias: 0,
            unknown1: 0,
            client_version: 134,
            field444: 444,
            field555: 555,
            permission_id: 1,
            reserved: [0; 12],
            cont_id: (kind == ClientKind::Continuum).then_some([0u8; 64]),
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        buf.clear();
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
        let settings = self
            .take(p)
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
        (p, settings)
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

/// The map CRC-32 this zone's arena has, which a client is expected to report
/// if it reports one at all.
fn map_crc() -> u32 {
    ArenaMap::from_bytes(&synthetic_map()).crc
}

/// A response every field of which is right for `key`.
///
/// `crc` is what goes in the Continuum tail: `Some` for a build that reports a
/// map CRC-32 (46 bytes on the wire), `None` for 0.40 (42). A VIE client has
/// no tail at all.
fn good(kind: ClientKind, settings: &[u8], key: u32, crc: Option<u32>) -> SecurityResponse {
    let block = SettingsBlock::from_bytes(settings.try_into().expect("1428 bytes"));
    SecurityResponse {
        weapon_count: 1,
        settings_checksum: block.checksum(key),
        // Without a `scrty` file the server does not check the Continuum exe
        // checksum, and the VIE one it computes itself.
        exe_checksum: match kind {
            ClientKind::Continuum => 0,
            _ => oss_proto::checksum::vie_exe_checksum(key),
        },
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
        continuum: (kind == ClientKind::Continuum).then_some((0, crc)),
    }
}

/// What happened to a client that answered one check.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Still playing well past the answer deadline.
    Accepted,
    /// Gone within a few ticks of the answer: kicked where it was diagnosed.
    KickedAtOnce,
    /// Gone only once the 15-second deadline passed, i.e. the answer was
    /// dropped on the floor and the player was then kicked for silence.
    KickedAsSilent,
}

/// Logs a client in, lets it be challenged, answers with `edit` applied to a
/// correct response, and says what became of it. `edit` may rewrite the
/// response, or encode the packet itself to send a length nothing would.
fn answer(
    kind: ClientKind,
    crc: Option<u32>,
    edit: impl FnOnce(&mut SecurityResponse, &mut Vec<u8>),
) -> Verdict {
    let mut h = H::new();
    let (p, settings) = h.player(kind);
    let key = h.next_challenge(p);
    let mut r = good(kind, &settings, key, crc);
    let mut buf = Vec::new();
    edit(&mut r, &mut buf);
    if buf.is_empty() {
        r.encode(&mut buf);
    }
    h.send(p, &buf);
    h.tick(5);
    if h.gone(p) {
        return Verdict::KickedAtOnce;
    }
    // Past the answer deadline, with room to spare, but short of the next
    // check so nothing else can be the cause.
    h.tick(ANSWER_TIME + 500);
    if h.gone(p) {
        Verdict::KickedAsSilent
    } else {
        Verdict::Accepted
    }
}

#[test]
fn continuum_42_byte_response_is_accepted() {
    // What Continuum 0.40 sends: the timer drift, no map CRC-32.
    assert_eq!(
        answer(ClientKind::Continuum, None, |r, buf| {
            r.encode(buf);
            assert_eq!(buf.len(), SecurityResponse::CONT_LEN);
        }),
        Verdict::Accepted
    );
}

#[test]
fn continuum_46_byte_response_is_accepted() {
    // What later builds send: the drift and a map CRC-32.
    assert_eq!(
        answer(ClientKind::Continuum, Some(map_crc()), |r, buf| {
            r.encode(buf);
            assert_eq!(buf.len(), SecurityResponse::CONT_CRC_LEN);
        }),
        Verdict::Accepted
    );
}

#[test]
fn a_42_byte_response_is_still_checked() {
    // "Accepted" above has to mean decoded and compared, not ignored: the
    // same length with one field spoiled must be kicked.
    assert_eq!(
        answer(ClientKind::Continuum, None, |r, _| r.settings_checksum ^= 1),
        Verdict::KickedAtOnce
    );
}

#[test]
fn a_length_no_client_sends_is_rejected_and_kicked() {
    // One byte short of the 0.40 form, and three short of it: neither is a
    // length any client is known to send.
    for drop in [1u64, 3] {
        let want = SecurityResponse::CONT_LEN - usize::try_from(drop).expect("small");
        assert_eq!(
            answer(ClientKind::Continuum, None, |r, buf| {
                r.encode(buf);
                buf.truncate(want);
            }),
            Verdict::KickedAtOnce,
            "a {want}-byte response must be kicked where it is diagnosed, not 15 s later as silence"
        );
    }
}

#[test]
fn a_vie_client_may_not_use_the_continuum_form() {
    // The gate is per client kind: 40 bytes for VIE, and the Continuum forms
    // are not VIE lengths.
    assert_eq!(
        answer(ClientKind::Vie, None, |r, buf| {
            r.continuum = Some((0, None));
            r.encode(buf);
            assert_eq!(buf.len(), SecurityResponse::CONT_LEN);
        }),
        Verdict::KickedAtOnce
    );
}

#[test]
fn a_client_that_reports_no_map_crc_is_not_a_crc_mismatch() {
    // The arena's CRC-32 is not zero, so reading "didn't report one" as
    // "reported zero" would kick every 0.40 client.
    assert_ne!(map_crc(), 0, "the test map must have a CRC worth comparing");
    assert_eq!(
        answer(ClientKind::Continuum, None, |_, _| {}),
        Verdict::Accepted
    );
    // And a client that does report one is still held to it.
    assert_eq!(
        answer(ClientKind::Continuum, Some(map_crc() ^ 1), |_, _| {}),
        Verdict::KickedAtOnce
    );
}
