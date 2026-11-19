//! `notify`: `?cheater` reaches the staff who are online, which is what the
//! chat module tells a player without staff chat to use.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::chat::{mask_type, set_player_chat_mask};
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
    /// `boss` is staff (sees staff chat); everyone else is not.
    fn new(global: &str) -> Self {
        let src = MemSource::new([
            (
                "conf/global.conf",
                format!("[Chat]\nMessageReliable = 1\n{global}"),
            ),
            (
                "conf/passwd.conf",
                format!(
                    "[General]\nAllowUnknown = yes\nAllowAnyLogin = yes\n[Users]\nboss = {}\n",
                    oss_server::modules::auth_file::hash_password("boss", "x")
                ),
            ),
            (
                "conf/groupdef.conf",
                // as zone/conf/groupdef.dir/default grants cmd_cheater
                "[default]\ncmd_cheater\ncmd_sos\ncmd_report\n\
                 [mod]\ncmd_cheater\nhigher_than_default\nseemodchat\nsendmodchat\n"
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
        self.lo.take(p);
        p
    }

    fn say(&mut self, from: PlayerId, text: &str) {
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind: chat_type::PUBLIC,
            sound: 0,
            pid: 0,
            text: text.as_bytes().to_vec(),
        })
        .encode(&mut buf);
        self.zone.handle_packet(from, &buf);
        self.lo.collect(&mut self.zone);
    }

    /// Every chat line `p` has been sent since last asked, with its type.
    fn heard(&mut self, p: PlayerId) -> Vec<(u8, String)> {
        self.lo
            .take(p)
            .into_iter()
            .filter(|(d, _)| d[0] == s2c::kind::CHAT)
            .map(|(d, _)| S2CChat::decode(&d).expect("chat").0)
            .map(|c| (c.kind, String::from_utf8_lossy(&c.text).into_owned()))
            .collect()
    }
}

#[test]
fn cheater_reaches_online_staff_and_only_them() {
    let mut h = H::new("");
    let boss = h.player("boss");
    let bystander = h.player("bystander");
    let reporter = h.player("reporter");

    // the advert a player without staff chat is given now names a real command
    h.say(reporter, "\\hello staff");
    assert!(
        h.heard(reporter)
            .iter()
            .any(|(_, t)| t.ends_with("use ?cheater."))
    );

    h.say(reporter, "?cheater someone is warping");
    assert_eq!(
        h.heard(reporter),
        vec![(
            chat_type::ARENA,
            "Message has been sent to online staff.".to_owned()
        )]
    );
    assert_eq!(
        h.heard(boss),
        vec![(
            chat_type::SERVER_ERROR,
            "cheater: (reporter) (Public 0): someone is warping".to_owned()
        )]
    );
    assert!(h.heard(bystander).is_empty());
}

#[test]
fn an_empty_alert_is_refused() {
    let mut h = H::new("");
    let boss = h.player("boss");
    let reporter = h.player("reporter");
    h.say(reporter, "?cheater   ");
    assert_eq!(
        h.heard(reporter),
        vec![(
            chat_type::ARENA,
            "Please include a message to send to online staff.".to_owned()
        )]
    );
    assert!(h.heard(boss).is_empty());
}

#[test]
fn alert_commands_and_the_empty_reply_come_from_config() {
    let mut h = H::new("[Notify]\nAlertCommand = report,sos\nEmptyReply = Say what?\n");
    let boss = h.player("boss");
    let reporter = h.player("reporter");

    h.say(reporter, "?sos stuck in a wall");
    assert_eq!(
        h.heard(boss),
        vec![(
            chat_type::SERVER_ERROR,
            "sos: (reporter) (Public 0): stuck in a wall".to_owned()
        )]
    );
    h.heard(reporter);
    h.say(reporter, "?report");
    assert_eq!(
        h.heard(reporter),
        vec![(chat_type::ARENA, "Say what?".to_owned())]
    );
    // the default is replaced, not added to
    h.heard(boss);
    h.say(reporter, "?cheater anyone");
    assert!(h.heard(boss).is_empty());
}

#[test]
fn a_player_silenced_from_staff_chat_cannot_alert() {
    let mut h = H::new("");
    let boss = h.player("boss");
    let reporter = h.player("reporter");
    set_player_chat_mask(&mut h.zone, reporter, 1 << mask_type::MODCHAT, 0);
    h.say(reporter, "?cheater spam spam spam");
    assert!(h.heard(boss).is_empty());
    assert!(h.heard(reporter).is_empty());
}
