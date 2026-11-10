//! `objects`: LVZ object state, the `?obj*` commands, the packets they send
//! and what a late-joiner is told.

use oss_formats::lvz::{CLV2, Lvz, ObjectSection, Section};
use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login, Rebroadcast};
use oss_proto::common::{ObjectChange, ObjectToggle, chat_type, object_change};
use oss_proto::objects::{ObjectData, layer, mode};
use oss_proto::s2c::{self, ChatBody, MoveObjects, S2CChat, ToggleObjects};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

/// Object 1 is a server-controlled map object, 2 a screen object, 3 a map
/// object that hides itself after 5 seconds.
fn fixture() -> Vec<u8> {
    let mut one = ObjectData::map(1, 800, 1600);
    one.image = 4;
    one.layer = layer::AFTER_TILES;
    one.set_mode(mode::SERVER_CONTROLLED);

    let mut two = ObjectData::screen(2, oss_proto::objects::reference::CENTER, -20, 0, 30);
    two.image = 7;
    two.set_mode(mode::SERVER_CONTROLLED);

    let mut three = ObjectData::map(3, 100, 200);
    three.set_display_time(50);
    three.set_mode(mode::SHOW_ALWAYS);

    let section = ObjectSection {
        version: CLV2,
        objects: vec![one, two, three],
        images: Vec::new(),
    };
    Lvz {
        sections: vec![Section::objects(&section)],
    }
    .to_bytes()
}

const ARENA: &str = "[General]\nLevelFiles = objs.lvz\n[Team]\nDesiredTeams = 2\n";

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new(caps: &str) -> Self {
        let mut src = MemSource::new([
            ("conf/global.conf", "[General]\n".to_owned()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", format!("[default]\n{caps}")),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", ARENA.to_owned()),
        ]);
        src.insert("maps/objs.lvz", fixture());
        let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        modules.push("objects".to_owned());
        let mut zone = ServerBuilder::with_source(src)
            .modules(modules)
            .build_zone()
            .expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
        }
    }

    /// Everything the ten object commands need, plus relaying.
    fn staffed() -> Self {
        Self::new(
            "cmd_objon = 1\nprivcmd_objon = 1\ncmd_objoff = 1\nprivcmd_objoff = 1\n\
             cmd_objset = 1\nprivcmd_objset = 1\ncmd_objmove = 1\nprivcmd_objmove = 1\n\
             cmd_objimage = 1\nprivcmd_objimage = 1\ncmd_objlayer = 1\nprivcmd_objlayer = 1\n\
             cmd_objtimer = 1\nprivcmd_objtimer = 1\ncmd_objmode = 1\nprivcmd_objmode = 1\n\
             cmd_objinfo = 1\ncmd_objlist = 1\ncmd_go = 1\n",
        )
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
        self.send(p, &buf);
        self.tick(10);
        buf.clear();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            arena: FixedStr::default(),
            ..Default::default()
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(20);
        p
    }

    /// Runs a command and returns everything queued for `p` afterwards.
    fn cmd(&mut self, p: PlayerId, text: &str) -> Vec<Vec<u8>> {
        self.cmd_to(p, 0, chat_type::PUBLIC, text)
    }

    fn cmd_to(&mut self, p: PlayerId, pid: i16, kind: u8, text: &str) -> Vec<Vec<u8>> {
        self.lo.take(p);
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind,
            sound: 0,
            pid,
            text: text.as_bytes().to_vec(),
        })
        .encode(&mut buf);
        self.send(p, &buf);
        self.take(p)
    }

    fn take(&mut self, p: PlayerId) -> Vec<Vec<u8>> {
        self.lo.take(p).into_iter().map(|(d, _)| d).collect()
    }
}

fn texts(packets: &[Vec<u8>]) -> Vec<String> {
    packets
        .iter()
        .filter(|d| d[0] == s2c::kind::CHAT)
        .map(|d| String::from_utf8_lossy(&S2CChat::decode(d).expect("chat").0.text).into_owned())
        .collect()
}

fn toggles(packets: &[Vec<u8>]) -> Vec<(u16, bool)> {
    packets
        .iter()
        .filter(|d| d[0] == s2c::kind::TOGGLE_OBJECTS)
        .flat_map(|d| ToggleObjects::decode(d).expect("0x35").toggles)
        .map(|t| (t.id(), t.on()))
        .collect()
}

fn changes(packets: &[Vec<u8>]) -> Vec<ObjectChange> {
    packets
        .iter()
        .filter(|d| d[0] == s2c::kind::MOVE_OBJECTS)
        .flat_map(|d| MoveObjects::decode(d).expect("0x36").changes)
        .collect()
}

#[test]
fn objlist_names_the_server_controlled_objects() {
    let mut h = H::staffed();
    let p = h.player("sysop");
    let lines = texts(&h.cmd(p, "?objlist"));
    assert_eq!(lines[0], "2 ServerControlled objects:");
    assert_eq!(lines[1], "1, 2");
}

#[test]
fn objinfo_describes_map_and_screen_objects() {
    let mut h = H::staffed();
    let p = h.player("sysop");
    let lines = texts(&h.cmd(p, "?objinfo 1"));
    assert_eq!(
        lines[0],
        "lvz: Id:1 Off:1 Image:4 Layer:AfterTiles Mode:ServerControlled"
    );
    assert_eq!(lines[1], "lvz: map object coords (800, 1600)");

    let lines = texts(&h.cmd(p, "?objinfo 2"));
    assert!(lines[1].starts_with("lvz: screen object coords (-20, 30)."));
    assert!(lines[1].contains("X-offset: C - Screen center"));

    assert_eq!(
        texts(&h.cmd(p, "?objinfo 99"))[0],
        "Object 99 does not exist in any of the loaded LVZ files."
    );
}

#[test]
fn objon_and_objoff_toggle_the_arena() {
    let mut h = H::staffed();
    let p = h.player("sysop");
    assert_eq!(toggles(&h.cmd(p, "?objon 1")), vec![(1, true)]);
    assert_eq!(toggles(&h.cmd(p, "?objoff 1")), vec![(1, false)]);
    // an id no LVZ file declares is still sent: ASSS does not check either
    assert_eq!(toggles(&h.cmd(p, "?objon 99")), vec![(99, true)]);
}

#[test]
fn objset_toggles_every_item_it_was_given() {
    let mut h = H::staffed();
    let p = h.player("sysop");
    // ASSS's own parser loses everything after the first item
    assert_eq!(
        toggles(&h.cmd(p, "?objset +1 -2 +3")),
        vec![(1, true), (2, false), (3, true)]
    );
    let out = h.cmd(p, "?objset +1 -2");
    let n = out
        .iter()
        .filter(|d| d[0] == s2c::kind::TOGGLE_OBJECTS)
        .count();
    assert_eq!(n, 1, "one packet, not one per object");
}

#[test]
fn a_late_joiner_is_told_what_changed() {
    let mut h = H::staffed();
    let first = h.player("sysop");
    h.cmd(first, "?objon 1");
    h.cmd(first, "?objimage 1 9");
    h.cmd(first, "?objlayer 1 TopMost");

    let second = h.player("latecomer");
    let seen = h.take(second);
    assert_eq!(toggles(&seen), vec![(1, true)]);
    let changed = changes(&seen);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].data.id(), 1);
    assert_eq!(changed[0].data.image, 9);
    assert_eq!(changed[0].data.layer, layer::TOP_MOST);
    // the position was never touched, so it still reads as the file had it
    assert_eq!(
        (changed[0].data.map_x(), changed[0].data.map_y()),
        (800, 1600)
    );
}

#[test]
fn nothing_is_replayed_when_nothing_changed() {
    let mut h = H::staffed();
    let _first = h.player("sysop");
    let second = h.player("latecomer");
    let seen = h.take(second);
    assert!(toggles(&seen).is_empty());
    assert!(changes(&seen).is_empty());
}

#[test]
fn turning_an_object_off_again_drops_it_from_the_replay() {
    let mut h = H::staffed();
    let first = h.player("sysop");
    h.cmd(first, "?objon 1");
    h.cmd(first, "?objoff 1");
    let second = h.player("latecomer");
    assert!(toggles(&h.take(second)).is_empty());
}

#[test]
fn an_object_with_a_display_time_is_not_remembered_as_shown() {
    let mut h = H::staffed();
    let first = h.player("sysop");
    // object 3 hides itself after five seconds, so the client's own timer
    // decides when it goes; ASSS does not record it as on, and nor do we
    assert_eq!(toggles(&h.cmd(first, "?objon 3")), vec![(3, true)]);
    let second = h.player("latecomer");
    assert!(toggles(&h.take(second)).is_empty());
}

#[test]
fn a_private_change_leaves_the_arena_alone() {
    let mut h = H::staffed();
    let first = h.player("sysop");
    let other = h.player("other");
    let other_pid = i16::try_from(other.0).expect("pid");

    h.lo.take(other);
    h.cmd_to(first, other_pid, chat_type::PRIVATE, "?objimage 1 3");
    // ASSS drops this one on the floor: its property commands only act on an
    // arena target, although the help says "Targets: any"
    let seen = h.take(other);
    assert_eq!(changes(&seen).len(), 1);
    assert_eq!(changes(&seen)[0].data.image, 3);

    let third = h.player("latecomer");
    assert!(changes(&h.take(third)).is_empty());
}

#[test]
fn objmove_knows_a_map_object_from_a_screen_object() {
    let mut h = H::staffed();
    let p = h.player("sysop");

    let changed = changes(&h.cmd(p, "?objmove 1 C40 50"));
    assert_eq!(changed[0].mask, object_change::POSITION);
    // object 1 is a map object, so the C is ignored and both are pixels
    assert!(changed[0].data.is_map_object());
    assert_eq!((changed[0].data.map_x(), changed[0].data.map_y()), (40, 50));

    let changed = changes(&h.cmd(p, "?objmove 2 C40 R-50"));
    assert!(!changed[0].data.is_map_object());
    assert_eq!(changed[0].data.screen_x(), 40);
    assert_eq!(changed[0].data.screen_y(), -50);
    assert_eq!(
        changed[0].data.x_ref(),
        oss_proto::objects::reference::CENTER
    );
    assert_eq!(
        changed[0].data.y_ref(),
        oss_proto::objects::reference::RADAR_TOP_LEFT
    );
}

#[test]
fn the_property_commands_each_set_their_own_bit() {
    let mut h = H::staffed();
    let p = h.player("sysop");
    for (text, mask) in [
        ("?objimage 1 2", object_change::IMAGE),
        ("?objlayer 1 AfterChat", object_change::LAYER),
        ("?objtimer 1 25", object_change::TIME),
        ("?objmode 1 EnterZone", object_change::MODE),
    ] {
        let changed = changes(&h.cmd(p, text));
        assert_eq!(changed.len(), 1, "{text}");
        assert_eq!(changed[0].mask, mask, "{text}");
    }
    // each change was kept, so they accumulate rather than resetting
    let lines = texts(&h.cmd(p, "?objinfo 1"));
    assert_eq!(
        lines[0],
        "lvz: Id:1 Off:1 Image:2 Layer:AfterChat Mode:EnterZone"
    );
}

#[test]
fn a_change_to_an_unknown_object_sends_nothing() {
    let mut h = H::staffed();
    let p = h.player("sysop");
    assert!(changes(&h.cmd(p, "?objimage 99 2")).is_empty());
}

#[test]
fn bad_arguments_are_refused_rather_than_read_as_object_zero() {
    let mut h = H::staffed();
    let p = h.player("sysop");
    let out = h.cmd(p, "?objon spoon");
    assert!(toggles(&out).is_empty());
    assert_eq!(
        texts(&out).first().map(String::as_str),
        Some("Invalid object id. Please read help for ?objon")
    );
    assert!(changes(&h.cmd(p, "?objlayer 1 Underneath")).is_empty());
    assert!(changes(&h.cmd(p, "?objmove 1 40")).is_empty());
}

/// A bot's C2S 0x0A carrying an object packet for the whole arena.
fn relay(h: &mut H, from: PlayerId, packet: Vec<u8>) {
    let mut buf = Vec::new();
    Rebroadcast::new(-1, packet)
        .expect("packet")
        .encode(&mut buf);
    h.send(from, &buf);
}

#[test]
fn relaying_needs_a_capability() {
    let mut h = H::staffed();
    let bot = h.player("bot");
    let watcher = h.player("watcher");
    h.lo.take(watcher);

    let mut pkt = Vec::new();
    ToggleObjects {
        toggles: vec![ObjectToggle::new(1, true)],
    }
    .encode(&mut pkt);
    relay(&mut h, bot, pkt);
    assert!(toggles(&h.take(watcher)).is_empty());
}

#[test]
fn a_bot_toggle_reaches_the_arena_and_is_remembered() {
    let mut h = H::new("cmd_go = 1\nbroadcastbot = 1\n");
    let bot = h.player("bot");
    let watcher = h.player("watcher");
    h.lo.take(watcher);

    let mut pkt = Vec::new();
    ToggleObjects {
        toggles: vec![ObjectToggle::new(1, true), ObjectToggle::new(2, false)],
    }
    .encode(&mut pkt);
    relay(&mut h, bot, pkt);
    assert_eq!(toggles(&h.take(watcher)), vec![(1, true), (2, false)]);

    let late = h.player("latecomer");
    assert_eq!(toggles(&h.take(late)), vec![(1, true)]);
}

#[test]
fn a_bot_move_is_remembered() {
    let mut h = H::new("cmd_go = 1\nbroadcastbot = 1\n");
    let bot = h.player("bot");

    let mut data = ObjectData::map(1, 800, 1600);
    data.image = 12;
    let mut pkt = Vec::new();
    MoveObjects {
        changes: vec![ObjectChange {
            mask: object_change::IMAGE,
            data,
        }],
    }
    .encode(&mut pkt);
    relay(&mut h, bot, pkt);

    let late = h.player("latecomer");
    let changed = changes(&h.take(late));
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].data.image, 12);
}

#[test]
fn a_bot_may_not_relay_other_packets_without_broadcastany() {
    let mut h = H::new("cmd_go = 1\nbroadcastbot = 1\n");
    let bot = h.player("bot");
    let watcher = h.player("watcher");
    h.lo.take(watcher);

    // a chat packet is not an object packet
    relay(
        &mut h,
        bot,
        vec![s2c::kind::CHAT, 0, 0, 0, 0, b'h', b'i', 0],
    );
    assert!(h.take(watcher).is_empty());

    let mut h = H::new("cmd_go = 1\nbroadcastany = 1\n");
    let bot = h.player("bot");
    let watcher = h.player("watcher");
    h.lo.take(watcher);
    relay(
        &mut h,
        bot,
        vec![s2c::kind::CHAT, 0, 0, 0, 0, b'h', b'i', 0],
    );
    assert_eq!(h.take(watcher).len(), 1);
}

#[test]
fn a_malformed_relay_is_dropped() {
    let mut h = H::new("cmd_go = 1\nbroadcastbot = 1\n");
    let bot = h.player("bot");
    let watcher = h.player("watcher");
    h.lo.take(watcher);

    // 0x36 with a part-sized record
    relay(&mut h, bot, vec![s2c::kind::MOVE_OBJECTS, 1, 2, 3]);
    assert!(h.take(watcher).is_empty());
    // and a packet with nothing but a type byte for 0x35
    relay(&mut h, bot, vec![s2c::kind::TOGGLE_OBJECTS]);
    assert!(h.take(watcher).is_empty());
}

#[test]
fn an_arena_with_no_lvz_files_still_works() {
    let mut src = MemSource::new([
        ("conf/global.conf", "[General]\n".to_owned()),
        (
            "conf/passwd.conf",
            "[General]\nAllowUnknown = yes\n".to_owned(),
        ),
        (
            "conf/groupdef.conf",
            "[default]\ncmd_objon = 1\ncmd_objlist = 1\ncmd_go = 1\n".to_owned(),
        ),
        ("conf/staff.conf", String::new()),
        (
            "arenas/(default)/arena.conf",
            "[Team]\nDesiredTeams = 2\n".to_owned(),
        ),
    ]);
    src.insert("maps/unused.lvz", Vec::<u8>::new());
    let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    modules.push("objects".to_owned());
    let zone = ServerBuilder::with_source(src)
        .modules(modules)
        .build_zone()
        .expect("zone");
    let mut h = H {
        zone,
        lo: Loopback::default(),
        t: 0,
    };
    h.zone.res.insert(oss_server::prng::Prng::new(1));
    let p = h.player("sysop");
    assert_eq!(
        texts(&h.cmd(p, "?objlist"))[0],
        "0 ServerControlled objects."
    );
    // and a toggle for an object the arena doesn't have is still relayed on
    assert_eq!(toggles(&h.cmd(p, "?objon 1")), vec![(1, true)]);
}
