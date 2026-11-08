//! M3 server behaviour over the loopback transport: seeds on entry, kills
//! with points and prizes, scores, shared greens, spectating players who die
//! without firing, and the periodic security check (answered correctly by
//! one client, ignored by another, which is kicked).

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::position::{C2SPosition, WeaponInfo, weapon_type};
use oss_proto::s2c;
use oss_proto::wire::FixedStr;

use oss_server::clientset::SettingsBlock;
use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const ARENA: &str = "[Modules]\nAttachModules = points_kill\n\
[PrizeWeight]\nQuickCharge = 1\n\
[Team]\nDesiredTeams = 2\n";

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new() -> Self {
        let src = MemSource::new([
            ("conf/global.conf", "[Security]\nSecurityKickoff = 1\n"),
            ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
            ("conf/groupdef.conf", "[default]\n"),
            ("conf/staff.conf", ""),
            ("arenas/(default)/arena.conf", ARENA),
        ]);
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

    fn player(&mut self, name: &str) -> (PlayerId, Vec<Vec<u8>>) {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        let login = Login {
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
            field555: 555,
            permission_id: 1,
            reserved: [0; 12],
            cont_id: None,
        };
        let mut buf = Vec::new();
        login.encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        let go = GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            ..Default::default()
        };
        buf.clear();
        go.encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        let pkts = self.take(p);
        (p, pkts)
    }

    fn position(&mut self, p: PlayerId, x: i16, weapon: u8) {
        let mut w = WeaponInfo::default();
        w.set_kind(weapon);
        let pos = C2SPosition {
            time: self.t as u32,
            x,
            y: 500,
            weapon: w,
            ..Default::default()
        };
        let mut buf = Vec::new();
        pos.encode(&mut buf);
        self.send(p, &buf);
    }

    fn die(&mut self, p: PlayerId, killer: PlayerId, bounty: i16) {
        let mut buf = Vec::new();
        c2s::Death {
            killer: killer.0 as i16,
            bounty,
        }
        .encode(&mut buf);
        self.send(p, &buf);
    }
}

fn find(pkts: &[Vec<u8>], kind: u8) -> Vec<Vec<u8>> {
    pkts.iter().filter(|d| d[0] == kind).cloned().collect()
}

#[test]
fn kills_scores_greens_and_security() {
    let mut h = H::new();
    let (a, apkts) = h.player("alpha");
    let (b, bpkts) = h.player("bravo");

    // seeds on entry, no checksum key
    for pk in [&apkts, &bpkts] {
        let sec = find(pk, s2c::kind::SECURITY);
        assert_eq!(sec.len(), 1);
        let sec = s2c::Security::decode(&sec[0]).expect("0x18");
        assert_eq!(sec.key, 0);
    }
    let settings = SettingsBlock::from_bytes(
        find(&apkts, s2c::kind::SETTINGS)[0]
            .as_slice()
            .try_into()
            .expect("1428 bytes"),
    );

    // both take ships: balanced onto freqs 0 and 1; a fires once
    h.send(a, &[c2s::kind::SET_SHIP, 0]);
    h.send(b, &[c2s::kind::SET_SHIP, 1]);
    assert_eq!(h.zone.players.get(a).map(|p| p.freq), Some(0));
    assert_eq!(h.zone.players.get(b).map(|p| p.freq), Some(1));
    h.position(a, 500, weapon_type::BULLET);
    h.position(b, 520, weapon_type::NONE);
    h.take(a);
    h.take(b);

    // b dies to a with bounty 5: points_kill awards 5, the only weighted
    // prize (QuickCharge = 1) is left behind, everyone is told
    h.die(b, a, 5);
    for p in [a, b] {
        let kill = find(&h.take(p), s2c::kind::KILL);
        let k = s2c::Kill::decode(&kill[0]).expect("0x06");
        assert_eq!(
            (k.killer, k.killed, k.bounty, k.green),
            (a.0 as i16, b.0 as i16, 5, 1)
        );
    }
    let score = h.zone.players.get(a).map(|p| p.score).expect("a");
    assert_eq!((score.kill_points, score.wins), (5, 1));
    assert_eq!(h.zone.players.get(b).map(|p| p.score.losses), Some(1));
    // scores go out within 5 seconds
    h.tick(500);
    let ups = find(&h.take(b), s2c::kind::SCORE_UPDATE);
    let a_up = ups
        .iter()
        .map(|u| s2c::ScoreUpdate::decode(u).expect("0x09"))
        .find(|u| u.pid == a.0 as i16)
        .expect("a's score update");
    assert_eq!((a_up.kill_points, a_up.kills), (5, 1));

    // a bad killer id is ignored
    h.die(b, PlayerId(999), 5);
    assert!(find(&h.take(a), s2c::kind::KILL).is_empty());

    // greens are shared with the arena
    let mut buf = Vec::new();
    c2s::TakeGreen {
        time: 1,
        x: 10,
        y: 20,
        prize: 3,
    }
    .encode(&mut buf);
    h.send(a, &buf);
    let g = find(&h.take(b), s2c::kind::GREEN);
    let g = s2c::Green::decode(&g[0]).expect("0x08");
    assert_eq!((g.pid, g.x, g.y, g.prize), (a.0 as i16, 10, 20, 3));
    assert!(find(&h.take(a), s2c::kind::GREEN).is_empty());

    // b never fires: the 6th death in a row puts b in spec
    for _ in 0..4 {
        h.die(b, a, 0);
    }
    assert_eq!(h.zone.players.get(b).map(|p| p.ship.0), Some(1));
    h.die(b, a, 0);
    assert_eq!(h.zone.players.get(b).map(|p| p.ship.0), Some(8));
    h.take(a);
    h.take(b);

    // the security check: new seeds and a key to both (they sent positions)
    h.tick(2500 - (h.t % 6000));
    let sec = find(&h.take(a), s2c::kind::SECURITY);
    let sec = s2c::Security::decode(sec.last().expect("security check")).expect("0x18");
    assert_ne!(sec.key, 0);
    assert_eq!(find(&h.take(b), s2c::kind::SECURITY).len(), 1);
    // a answers correctly: settings, (empty) map and VIE exe checksums
    let resp = c2s::SecurityResponse {
        weapon_count: 1,
        settings_checksum: settings.checksum(sec.key),
        exe_checksum: oss_proto::checksum::vie_exe_checksum(sec.key),
        map_checksum: oss_server::modules::mapdata::ArenaMap::from_bytes(&[]).checksum(sec.key),
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
    };
    buf.clear();
    resp.encode(&mut buf);
    assert_eq!(buf.len(), c2s::SecurityResponse::VIE_LEN);
    h.send(a, &buf);
    // b stays silent and is kicked when the answer time runs out
    h.tick(1600);
    assert!(h.zone.players.get(a).is_some(), "correct answer keeps a");
    assert!(
        h.zone.players.get(b).is_none(),
        "no answer kicks b (SecurityKickoff)"
    );
}

/// Scores survive leaving and logging back in (persist, in-memory SQLite
/// for zones without a directory).
#[test]
fn scores_persist_across_logins() {
    let mut h = H::new();
    let (a, _) = h.player("alpha");
    let (b, _) = h.player("bravo");
    h.send(a, &[c2s::kind::SET_SHIP, 0]);
    h.send(b, &[c2s::kind::SET_SHIP, 1]);
    h.position(a, 500, weapon_type::BULLET);
    h.die(b, a, 7);
    assert_eq!(h.zone.players.get(a).map(|p| p.score.kill_points), Some(7));
    core_mod::kick(&mut h.zone, a);
    h.tick(20);
    assert!(h.zone.players.get(a).is_none());
    let (a2, pkts) = h.player("Alpha");
    assert_eq!(
        h.zone
            .players
            .get(a2)
            .map(|p| (p.score.kill_points, p.score.wins)),
        Some((7, 1))
    );
    // other players see the restored score in the entering record
    let me = find(&pkts, s2c::kind::PLAYER_ENTERING)
        .iter()
        .map(|d| s2c::PlayerEntering::decode(d).expect("0x03").players[0].clone())
        .find(|p| p.name.as_bytes() == b"Alpha")
        .expect("own record");
    assert_eq!(me.kill_points, 7);
}
