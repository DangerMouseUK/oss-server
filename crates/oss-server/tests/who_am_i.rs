//! S2C `0x01` ("you are"): a Continuum client is told the arena's name, or an
//! empty name for a public arena; a VIE client gets the 3-byte form ASSS
//! sends to everyone.

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
    fn new() -> Self {
        let src = MemSource::new([
            ("conf/global.conf", "[General]\n".to_owned()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\nAllowAnyLogin = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\ncmd_go = 1\n".to_owned()),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", String::new()),
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

    fn login(&mut self, name: &str, kind: ClientKind) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, kind, None).expect("slot");
        let continuum = kind == ClientKind::Continuum;
        let mut buf = Vec::new();
        Login {
            kind: if continuum {
                c2s::kind::CONT_LOGIN
            } else {
                c2s::kind::LOGIN
            },
            flags: 0,
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(b"x"),
            machine_id: 1,
            connect_type: 4,
            timezone_bias: 0,
            unknown1: 0,
            client_version: if continuum { 40 } else { 134 },
            field444: 444,
            bound_ip: 0,
            permission_id: 1,
            server_ip: 0,
            bound_port: 0,
            redirect_id: 0,
            using_proxy: 0,
            unused: 0,
            cont_id: continuum.then_some([0u8; 64]),
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        self.lo.take(p);
        p
    }

    /// Sends the player to `arena` ("" for a public arena) and returns the
    /// S2C `0x01` packets it was sent.
    fn go(&mut self, p: PlayerId, arena: &str) -> Vec<Vec<u8>> {
        let mut buf = Vec::new();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: if arena.is_empty() { -1 } else { -3 },
            arena: FixedStr::new(arena.as_bytes()),
            ..Default::default()
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(20);
        self.lo
            .take(p)
            .into_iter()
            .map(|(d, _)| d)
            .filter(|d| d[0] == s2c::kind::PLAYER_ID)
            .collect()
    }
}

fn who_am_i(pid: PlayerId, name: Option<&[u8]>) -> Vec<u8> {
    let mut v = vec![s2c::kind::PLAYER_ID];
    v.extend_from_slice(&(pid.0 as i16).to_le_bytes());
    if let Some(n) = name {
        v.extend_from_slice(n);
        v.push(0);
    }
    v
}

#[test]
fn continuum_is_told_the_arena_name_and_vie_is_not() {
    let mut h = H::new();
    let cont = h.login("cont", ClientKind::Continuum);
    let vie = h.login("vie", ClientKind::Vie);

    // a public arena: an empty name (4 bytes), which is also what lets
    // Continuum save the player's ship
    assert_eq!(h.go(cont, ""), vec![who_am_i(cont, Some(b""))]);
    assert_eq!(h.go(vie, ""), vec![who_am_i(vie, None)]);

    // a named arena: its name, so Continuum uses the right file folder even
    // when it was put there rather than asking for it
    assert_eq!(h.go(cont, "duel"), vec![who_am_i(cont, Some(b"duel"))]);
    assert_eq!(h.go(vie, "duel"), vec![who_am_i(vie, None)]);

    // a numbered copy shares its base name's files, so the base name is sent
    assert_eq!(h.go(cont, "duel2"), vec![who_am_i(cont, Some(b"duel"))]);

    // and it decodes as Continuum reads it
    let pkt = s2c::PlayerId::decode(&h.go(cont, "duel")[0]).expect("0x01");
    assert_eq!(pkt.arena.as_deref(), Some(&b"duel"[..]));
}
