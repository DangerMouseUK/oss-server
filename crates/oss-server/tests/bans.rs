//! Bans: ASSS's `?kick [minutes]` machine-id bans, and `?ban` by name,
//! machine id and identity key, checked before authentication.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, LoginResponse, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::core::{self as core_mod, auth_code};
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
            (
                "conf/global.conf",
                "[Chat]\nMessageReliable = 1\n".to_owned(),
            ),
            (
                "conf/passwd.conf",
                format!(
                    "[General]\nAllowUnknown = yes\n[Users]\nboss = {}\n",
                    oss_server::modules::auth_file::hash_password("boss", "x")
                ),
            ),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_go = 1\nprivcmd_kick\n\
                 [mod]\ncmd_go = 1\nhigher_than_default\nprivcmd_kick\nprivcmd_ban\n\
                 cmd_banlist\ncmd_unban\ncmd_listmidbans\ncmd_delmidban\n"
                    .to_owned(),
            ),
            ("conf/staff.conf", "[(global)]\nboss = mod\n".to_owned()),
            ("arenas/(default)/arena.conf", String::new()),
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

    /// Logs in with a machine id (and optionally an identity key the
    /// connection proved); returns the player and the login result code.
    fn login(&mut self, name: &str, mid: u32, key: Option<[u8; 32]>) -> (PlayerId, u8) {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        if let Some(pl) = self.zone.players.get_mut(p) {
            pl.identity_key = key;
        }
        let mut buf = Vec::new();
        Login {
            kind: c2s::kind::LOGIN,
            flags: 0,
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(b"x"),
            machine_id: mid,
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
        let code = self
            .lo
            .take(p)
            .iter()
            .find(|(d, _)| d[0] == s2c::kind::LOGIN_RESPONSE)
            .map(|(d, _)| LoginResponse::decode(d).expect("lr").code)
            .expect("login response");
        if code == auth_code::OK {
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
            self.lo.take(p);
        }
        (p, code)
    }

    fn cmd(&mut self, from: PlayerId, to: Option<PlayerId>, text: &str) -> Vec<String> {
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind: if to.is_some() {
                chat_type::PRIVATE
            } else {
                chat_type::PUBLIC
            },
            sound: 0,
            pid: to.map_or(0, |t| t.0 as i16),
            text: text.as_bytes().to_vec(),
        })
        .encode(&mut buf);
        self.lo.take(from);
        self.zone.handle_packet(from, &buf);
        self.lo.collect(&mut self.zone);
        self.lo
            .take(from)
            .into_iter()
            .filter(|(d, _)| d[0] == s2c::kind::CHAT)
            .map(|(d, _)| {
                String::from_utf8_lossy(&S2CChat::decode(&d).expect("chat").0.text).into_owned()
            })
            .collect()
    }

    fn gone(&mut self, p: PlayerId) -> bool {
        self.tick(20);
        self.zone.players.get(p).is_none()
    }
}

#[test]
fn kick_and_machine_id_bans() {
    let mut h = H::new();
    let (boss, _) = h.login("boss", 1, None);
    let (alice, _) = h.login("alice", 1234, None);
    // alice can't kick a higher group
    let out = h.cmd(alice, Some(boss), "?kick");
    assert!(
        out.iter()
            .any(|t| t == "You don't have permission to use ?kick on that player.")
    );
    assert!(!h.gone(boss));

    h.cmd(boss, Some(alice), "?kick 5");
    assert!(h.gone(alice));
    // same machine, any name: locked out for 5 minutes
    assert_eq!(h.login("alice2", 1234, None).1, auth_code::LOCKED_OUT);
    assert_eq!(
        h.login("alice", 999, None).1,
        auth_code::OK,
        "kick bans the machine, not the name"
    );
    let out = h.cmd(boss, None, "?listmidbans");
    assert!(out.iter().any(|t| t.starts_with("1234 (")), "{out:?}");
    h.cmd(boss, None, "?delmidban 1234");
    assert_eq!(h.login("alice3", 1234, None).1, auth_code::OK);
    // a kick without minutes bans nothing
    let (bob, _) = h.login("bob", 55, None);
    h.cmd(boss, Some(bob), "?kick");
    assert!(h.gone(bob));
    assert_eq!(h.login("bob", 55, None).1, auth_code::OK);
}

#[test]
fn bans_follow_name_machine_and_key() {
    let mut h = H::new();
    let (boss, _) = h.login("boss", 1, None);
    let key = [9u8; 32];
    let (carol, _) = h.login("carol", 777, Some(key));
    let out = h.cmd(boss, Some(carol), "?ban -t 60 aimbot");
    assert!(
        out.iter()
            .any(|t| t == "Ban 1: carol (name, machine id, identity key)"),
        "{out:?}"
    );
    assert!(h.gone(carol));
    // by name (any case), by machine, by key: each is enough
    assert_eq!(h.login("CAROL", 1, None).1, auth_code::LOCKED_OUT);
    assert_eq!(h.login("other", 777, None).1, auth_code::LOCKED_OUT);
    assert_eq!(h.login("fresh", 4242, Some(key)).1, auth_code::LOCKED_OUT);
    assert_eq!(h.login("fresh", 4242, Some([1; 32])).1, auth_code::OK);
    let out = h.cmd(boss, None, "?banlist");
    assert!(
        out.iter()
            .any(|t| t.starts_with("1: carol mid 777 key 09090909")),
        "{out:?}"
    );
    let out = h.cmd(boss, None, "?unban 1");
    assert!(out.iter().any(|t| t == "Ban 1 removed."));
    assert_eq!(h.login("carol", 777, Some(key)).1, auth_code::OK);
}
