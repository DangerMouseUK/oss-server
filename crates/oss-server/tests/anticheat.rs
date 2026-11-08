//! The anti-cheat module: off by default, log-only unless told otherwise,
//! and no findings for a client playing at the limits of its ship.

use std::sync::{Arc, Mutex};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::position::{C2SPosition, WeaponInfo, weapon_type};
use oss_proto::s2c::{self, ChatBody, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::anticheat::{CheatSuspected, Check, trust};
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const SHIP: &str =
    "[Warbird]\nBulletFireDelay = 50\nMultiFireDelay = 50\nMaxGuns = 2\nMaxBombs = 1\n";

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    seen: Arc<Mutex<Vec<Check>>>,
}

impl H {
    fn new(ac: &str) -> Self {
        let arena = format!("{SHIP}[AntiCheat]\n{ac}");
        let src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_trust = 1\nprivcmd_trust = 1\n".to_owned(),
            ),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", arena),
        ]);
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        zone.on::<CheatSuspected>(Owner(999), Scope::Global, move |_, ev| {
            s.lock().expect("lock").push(ev.check);
        });
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            seen,
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

    fn player(&mut self, name: &str) -> PlayerId {
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
            field555: 555,
            permission_id: 1,
            reserved: [0; 12],
            cont_id: None,
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
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
        self.zone.handle_packet(p, &buf);
        self.tick(10);
        self.zone.handle_packet(p, &[c2s::kind::SET_SHIP, 0]);
        self.tick(1);
        p
    }

    /// A position with client time `time` (ticks), firing `kind` at `level`.
    fn pos(&mut self, p: PlayerId, time: u32, kind: u8, level: u8) {
        let mut w = WeaponInfo::default();
        w.set_kind(kind);
        w.set_level(level);
        let mut buf = Vec::new();
        C2SPosition {
            time,
            x: 8000,
            y: 8000,
            energy: 1000,
            weapon: w,
            ..Default::default()
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.lo.collect(&mut self.zone);
    }

    fn seen(&self) -> Vec<Check> {
        self.seen.lock().expect("lock").clone()
    }

    fn ship(&self, p: PlayerId) -> u8 {
        self.zone.players.get(p).map_or(99, |x| x.ship.0)
    }

    fn texts(&mut self, p: PlayerId) -> Vec<String> {
        self.lo
            .take(p)
            .into_iter()
            .filter(|(d, _)| d[0] == s2c::kind::CHAT)
            .map(|(d, _)| {
                String::from_utf8_lossy(&S2CChat::decode(&d).expect("chat").0.text).into_owned()
            })
            .collect()
    }
}

#[test]
fn off_by_default() {
    let mut h = H::new("");
    let p = h.player("alice");
    for i in 0..10 {
        h.pos(p, 1000 + i * 5, weapon_type::BULLET, 3);
    }
    assert!(h.seen().is_empty());
    assert_eq!(trust(&h.zone, p), 100);
}

#[test]
fn honest_play_at_the_limits() {
    let mut h = H::new("Enabled = 1\nAction = 2\n");
    let p = h.player("alice");
    // fire exactly at the ship's delay, at its highest gun level, for a minute,
    // with a little network jitter on top of a steady clock
    let start = h.t as u32;
    for i in 0..120u32 {
        let jitter = [0, 3, 1, 4, 2][i as usize % 5];
        h.tick(50);
        let t = start + i * 50 + jitter;
        h.pos(p, t, weapon_type::BULLET, 1);
    }
    assert!(h.seen().is_empty(), "{:?}", h.seen());
    assert_eq!(h.ship(p), 0);
}

#[test]
fn log_only_by_default_when_enabled() {
    let mut h = H::new("Enabled = 1\n");
    let p = h.player("alice");
    h.pos(p, 1000, weapon_type::BULLET, 0);
    h.pos(p, 1010, weapon_type::BULLET, 0);
    assert_eq!(h.seen(), vec![Check::FireRate]);
    assert_eq!(trust(&h.zone, p), 90);
    assert_eq!(h.ship(p), 0, "Action = 0 only logs");
    // a check counts once per cooldown
    h.pos(p, 1020, weapon_type::BULLET, 0);
    assert_eq!(h.seen().len(), 1);
    // over-level guns
    h.pos(p, 1200, weapon_type::BULLET, 2);
    assert_eq!(h.seen(), vec![Check::FireRate, Check::WeaponLevel]);
}

#[test]
fn spec_until_trust_recovers() {
    let mut h = H::new(
        "Enabled = 1\nAction = 1\nSpecTrust = 80\nWeaponLevelWeight = 30\nRecoverPerMinute = 60\n",
    );
    let p = h.player("alice");
    h.lo.take(p);
    h.pos(p, 1000, weapon_type::BULLET, 3);
    assert_eq!(trust(&h.zone, p), 70);
    assert_eq!(h.ship(p), 8);
    assert!(
        h.texts(p)
            .iter()
            .any(|t| t == "You have been moved to spectator mode by the anti-cheat checks.")
    );
    // kept out of ships meanwhile
    h.zone.handle_packet(p, &[c2s::kind::SET_SHIP, 1]);
    h.lo.collect(&mut h.zone);
    assert_eq!(h.ship(p), 8);
    // ?trust shows the finding
    let mut buf = Vec::new();
    c2s::C2SChat(ChatBody {
        kind: chat_type::PUBLIC,
        sound: 0,
        pid: 0,
        text: b"?trust".to_vec(),
    })
    .encode(&mut buf);
    h.texts(p);
    h.zone.handle_packet(p, &buf);
    h.lo.collect(&mut h.zone);
    let out = h.texts(p);
    assert!(
        out.iter().any(|t| t == "alice: trust 70 (kept in spec)"),
        "{out:?}"
    );
    assert!(out.iter().any(|t| t.contains("weapon level")), "{out:?}");
    // 1 point a second: back above 80 after about 10 s
    h.tick(1200);
    assert!(trust(&h.zone, p) >= 80);
    h.zone.handle_packet(p, &[c2s::kind::SET_SHIP, 1]);
    h.lo.collect(&mut h.zone);
    assert_eq!(h.ship(p), 1);
}

#[test]
fn fast_client_clock() {
    let mut h = H::new("Enabled = 1\nClockWindow = 10\nClockDrift = 5\n");
    let p = h.player("alice");
    let fast = h.player("bob");
    let start = h.t as u32;
    // 12 s of positions; bob's clock runs 20% fast
    for i in 0..121u32 {
        h.tick(10);
        h.pos(p, start + i * 10, weapon_type::NONE, 0);
        h.pos(fast, start + i * 12, weapon_type::NONE, 0);
    }
    assert_eq!(h.seen(), vec![Check::Clock]);
    assert_eq!(trust(&h.zone, p), 100);
    assert_eq!(trust(&h.zone, fast), 70);
}
