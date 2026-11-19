//! Keep-alives (S2C `0x27`) come often enough to beat the arena's
//! `Latency:S2CNoDataKickoutDelay`, and follow the player to a new arena.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::s2c;
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    /// The default arena has the stock kick-out of 500 ticks, `fast` 300 and
    /// `short` 150.
    fn new() -> Self {
        let src = MemSource::new([
            ("conf/global.conf", "[General]\n".to_owned()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\nAllowAnyLogin = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\ncmd_go = 1\n".to_owned()),
            ("conf/staff.conf", String::new()),
            (
                "arenas/(default)/arena.conf",
                "[Latency]\nS2CNoDataKickoutDelay = 500\n".to_owned(),
            ),
            (
                "arenas/fast/arena.conf",
                "[Latency]\nS2CNoDataKickoutDelay = 300\n".to_owned(),
            ),
            (
                "arenas/short/arena.conf",
                "[Latency]\nS2CNoDataKickoutDelay = 150\n".to_owned(),
            ),
        ]);
        let zone = ServerBuilder::with_source(src)
            .modules(oss_server::modules::DEFAULT_MODULES.iter().copied())
            .build_zone()
            .expect("zone");
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
        }
    }

    fn tick(&mut self) {
        self.t += 1;
        self.zone.set_now(Ticks(self.t));
        oss_server::timers::run_due(&mut self.zone);
        self.lo.collect(&mut self.zone);
    }

    fn send(&mut self, p: PlayerId, data: &[u8]) {
        self.zone.handle_packet(p, data);
        self.lo.collect(&mut self.zone);
    }

    fn go(&mut self, p: PlayerId, arena: &str) {
        let mut buf = Vec::new();
        GoArena {
            ship: 0,
            x_res: 1024,
            y_res: 768,
            arena_type: if arena.is_empty() { -1 } else { -3 },
            arena: FixedStr::new(arena.as_bytes()),
            ..Default::default()
        }
        .encode(&mut buf);
        self.send(p, &buf);
        for _ in 0..20 {
            self.tick();
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
        for _ in 0..10 {
            self.tick();
        }
        self.go(p, "");
        p
    }

    /// Runs `ticks` ticks and returns the gaps between the keep-alives `p`
    /// was sent in that time.
    fn keep_alive_gaps(&mut self, p: PlayerId, ticks: u64) -> Vec<u64> {
        self.lo.take(p);
        let mut at = Vec::new();
        for _ in 0..ticks {
            self.tick();
            if self
                .lo
                .take(p)
                .iter()
                .any(|(d, _)| d.as_slice() == [s2c::kind::KEEP_ALIVE])
            {
                at.push(self.t);
            }
        }
        at.windows(2).map(|w| w[1] - w[0]).collect()
    }
}

#[test]
fn keep_alives_follow_the_arena_kickout_delay() {
    let mut h = H::new();
    let p = h.player("lone");

    // stock 500-tick kick-out: every 400 ticks, not every 500
    let gaps = h.keep_alive_gaps(p, 2000);
    assert!(gaps.len() >= 3, "{gaps:?}");
    assert!(gaps.iter().all(|&g| (400..=410).contains(&g)), "{gaps:?}");

    // a 300-tick kick-out, after changing arena: every 200
    h.go(p, "fast");
    let gaps = h.keep_alive_gaps(p, 1500);
    assert!(gaps.len() >= 5, "{gaps:?}");
    assert!(gaps.iter().all(|&g| (200..=210).contains(&g)), "{gaps:?}");

    // too short to plan around (200 or less): ASSS's 500, as .NET leaves it
    h.go(p, "short");
    let gaps = h.keep_alive_gaps(p, 2000);
    assert!(gaps.len() >= 2, "{gaps:?}");
    assert!(gaps.iter().all(|&g| (500..=510).contains(&g)), "{gaps:?}");

    // and back again
    h.go(p, "");
    let gaps = h.keep_alive_gaps(p, 2000);
    assert!(gaps.iter().all(|&g| (400..=410).contains(&g)), "{gaps:?}");
}
