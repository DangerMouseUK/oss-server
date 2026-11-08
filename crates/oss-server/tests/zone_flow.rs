//! End-to-end game logic over the loopback transport: login (password file),
//! arena entry with the ASSS packet sequence, chat routing, commands with
//! capabilities, leaving, duplicate logins, kicks and arena reaping.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, LoginResponse, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::{ConfigFile, MemSource};
use oss_server::modules::auth_file::hash_password;
use oss_server::modules::core::{self as core_mod, auth_code};
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId, PlayerStatus};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const ARENA_CONF: &str = "[Warbird]\nInitialSpeed = 2000\n[Misc]\nBounceFactor = 22\n\
[Modules]\nAttachModules = \n";

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new() -> Self {
        Self::with_conf(ARENA_CONF, "[Chat]\nMessageReliable = 1\n")
    }

    fn with_conf(arena_conf: &str, global: &str) -> Self {
        let passwd = format!(
            "[General]\nAllowUnknown = yes\n[Users]\nbob = {}\nlocked = lock\n",
            hash_password("Bob", "secret")
        );
        let src = MemSource::new([
            ("conf/global.conf", global.to_owned()),
            ("conf/passwd.conf", passwd),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_version = 1\ncmd_go = 1\ncmd_help = 1\n\
                 privcmd_freqkick\nprivcmd_giveowner\ncmd_takeownership\n\
                 [mod]\ncmd_version = 1\ncmd_go = 1\nseemodchat\nsendmodchat\n\
                 cmd_arena\nprivcmd_setship\nprivcmd_lock\nprivcmd_prize\nprivcmd_info\n\
                 privcmd_warpto\ncmd_z\nhigher_than_default\n\
                 privcmd_freqkick\nprivcmd_giveowner\ncmd_takeownership\n"
                    .to_owned(),
            ),
            ("conf/staff.conf", "[(global)]\nbob = mod\n".to_owned()),
            ("arenas/(default)/arena.conf", arena_conf.to_owned()),
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

    fn connect(&mut self) -> PlayerId {
        core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot")
    }

    fn send(&mut self, p: PlayerId, data: &[u8]) {
        self.zone.handle_packet(p, data);
        self.lo.collect(&mut self.zone);
    }

    fn take(&mut self, p: PlayerId) -> Vec<Vec<u8>> {
        self.lo.take(p).into_iter().map(|(d, _)| d).collect()
    }

    fn login(&mut self, name: &str, pw: &str) -> (PlayerId, u8) {
        let p = self.connect();
        let mut buf = Vec::new();
        login_packet(name, pw).encode(&mut buf);
        assert_eq!(buf.len(), Login::VIE_LEN);
        self.send(p, &buf);
        self.tick(10);
        let code = self
            .take(p)
            .iter()
            .find(|d| d[0] == s2c::kind::LOGIN_RESPONSE)
            .map(|d| LoginResponse::decode(d).expect("login response").code)
            .expect("no login response");
        (p, code)
    }

    fn enter(&mut self, p: PlayerId) -> Vec<Vec<u8>> {
        let go = GoArena {
            ship: 8,
            allow_audio: 1,
            obscenity_filter: 0,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            arena: FixedStr::new(b""),
            optional_graphics: None,
        };
        let mut buf = Vec::new();
        go.encode(&mut buf);
        assert_eq!(buf.len(), 26);
        self.send(p, &buf);
        self.tick(10);
        self.take(p)
    }

    fn chat(&mut self, p: PlayerId, kind: u8, target: i16, text: &str) {
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind,
            sound: 0,
            pid: target,
            text: text.as_bytes().to_vec(),
        })
        .encode(&mut buf);
        self.send(p, &buf);
    }

    fn status(&self, p: PlayerId) -> Option<PlayerStatus> {
        self.zone.players.get(p).map(|p| p.status)
    }
}

fn login_packet(name: &str, pw: &str) -> Login {
    Login {
        kind: c2s::kind::LOGIN,
        flags: 0,
        name: FixedStr::new(name.as_bytes()),
        password: FixedStr::new(pw.as_bytes()),
        machine_id: 1234,
        connect_type: 4,
        timezone_bias: 0,
        unknown1: 0,
        client_version: 134,
        field444: 444,
        field555: 555,
        permission_id: 99,
        reserved: [0; 12],
        cont_id: None,
    }
}

fn chats(pkts: &[Vec<u8>]) -> Vec<ChatBody> {
    pkts.iter()
        .filter(|d| d[0] == s2c::kind::CHAT)
        .map(|d| S2CChat::decode(d).expect("chat").0)
        .collect()
}

#[test]
fn login_enter_chat_leave() {
    let mut h = H::new();

    // wrong password: rejected, stays connected
    let (carol, code) = h.login("bob", "wrong");
    assert_eq!(code, auth_code::BAD_PASSWORD);
    assert_eq!(h.status(carol), Some(PlayerStatus::Connected));
    // locked account
    let (_, code) = h.login("locked", "x");
    assert_eq!(code, auth_code::NO_PERMISSION);
    // bad VIE version
    let p = h.connect();
    let mut l = login_packet("vers", "x");
    l.client_version = 133;
    let mut buf = Vec::new();
    l.encode(&mut buf);
    h.send(p, &buf);
    h.tick(3);
    let lr = h
        .take(p)
        .into_iter()
        .find(|d| d[0] == s2c::kind::LOGIN_RESPONSE)
        .expect("lr");
    assert_eq!(
        LoginResponse::decode(&lr).expect("lr").code,
        auth_code::LOCKED_OUT
    );

    // bob: correct password, authenticated, staff group
    let (bob, code) = h.login("  Bob", "secret");
    assert_eq!(code, auth_code::OK);
    let pb = h.zone.players.get(bob).expect("bob");
    assert_eq!(pb.name, "Bob");
    assert!(pb.authenticated);
    assert_eq!(pb.status, PlayerStatus::LoggedIn);

    // entering: ASSS order of packets
    let pkts = h.enter(bob);
    let kinds: Vec<u8> = pkts.iter().map(|d| d[0]).collect();
    assert_eq!(
        kinds,
        vec![
            s2c::kind::PLAYER_ID,
            s2c::kind::SETTINGS,
            s2c::kind::PLAYER_ENTERING,
            s2c::kind::MAP_FILENAME,
            s2c::kind::BRICK,
            s2c::kind::ENTERING_ARENA,
            s2c::kind::SECURITY
        ]
    );
    assert_eq!(&pkts[0], &[0x01, bob.0 as u8, (bob.0 >> 8) as u8]);
    let want = oss_server::clientset::load(&ConfigFile::from_text(ARENA_CONF)).block;
    assert_eq!(pkts[1].as_slice(), want.as_bytes().as_slice());
    assert_eq!(h.status(bob), Some(PlayerStatus::Playing));
    let arena = h
        .zone
        .players
        .get(bob)
        .and_then(|p| p.arena)
        .expect("arena");
    assert_eq!(h.zone.arenas.get(arena).map(|a| a.name.as_str()), Some("0"));
    assert_eq!(h.zone.players.get(bob).map(|p| p.freq), Some(8025));

    // alice: unknown name allowed, not authenticated
    let (alice, code) = h.login("alice", "whatever");
    assert_eq!(code, auth_code::OK);
    assert!(!h.zone.players.get(alice).expect("alice").authenticated);
    let apkts = h.enter(alice);
    // alice learns about bob, bob learns about alice
    let entering: Vec<_> = apkts
        .iter()
        .filter(|d| d[0] == s2c::kind::PLAYER_ENTERING)
        .collect();
    assert_eq!(entering.len(), 2);
    let bpkts = h.take(bob);
    let seen = bpkts
        .iter()
        .find(|d| d[0] == s2c::kind::PLAYER_ENTERING)
        .expect("bob sees alice");
    let rec = s2c::PlayerEntering::decode(seen).expect("decode");
    assert_eq!(rec.players[0].name.as_bytes(), b"alice");

    // public chat reaches others, not the sender
    h.chat(alice, chat_type::PUBLIC, 0, "hello\x01");
    let got = chats(&h.take(bob));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].kind, chat_type::PUBLIC);
    assert_eq!(got[0].pid, alice.0 as i16);
    assert_eq!(got[0].text, b"hello_");
    assert!(chats(&h.take(alice)).is_empty());

    // private chat
    h.chat(bob, chat_type::PRIVATE, alice.0 as i16, "psst");
    let got = chats(&h.take(alice));
    assert_eq!(
        (got[0].kind, got[0].text.as_slice()),
        (chat_type::PRIVATE, b"psst".as_slice())
    );

    // commands: allowed for default group, answered with an arena message
    h.chat(alice, chat_type::PUBLIC, 0, "?version");
    let got = chats(&h.take(alice));
    assert_eq!(got[0].kind, chat_type::ARENA);
    assert!(String::from_utf8_lossy(&got[0].text).contains("Open SubSpace"));
    // a command nobody may use is silently refused
    h.chat(alice, chat_type::PUBLIC, 0, "?passwd new");
    assert!(chats(&h.take(alice)).is_empty());
    assert!(chats(&h.take(bob)).is_empty());

    // staff chat: bob (mod) may send; alice may not
    h.chat(alice, chat_type::PUBLIC, 0, "\\hi staff");
    let got = chats(&h.take(alice));
    assert!(String::from_utf8_lossy(&got[0].text).contains("aren't allowed"));

    // bob leaves the arena: alice is told
    h.send(bob, &[c2s::kind::LEAVE_ARENA]);
    h.tick(3);
    let leaving = h
        .take(alice)
        .into_iter()
        .find(|d| d[0] == s2c::kind::PLAYER_LEAVING)
        .expect("leaving");
    assert_eq!(leaving, vec![0x04, bob.0 as u8, (bob.0 >> 8) as u8]);
    assert_eq!(h.status(bob), Some(PlayerStatus::LoggedIn));

    // logging in again as bob replaces the old bob
    let (bob2, code) = h.login("BOB", "secret");
    assert_eq!(code, auth_code::OK);
    assert!(h.zone.players.get(bob).is_none(), "old bob freed");
    assert!(h.lo.take_disconnects().iter().any(|d| d.player == bob));
    assert_eq!(h.status(bob2), Some(PlayerStatus::LoggedIn));

    // kick alice: she leaves the arena and the zone; the empty arena is reaped
    core_mod::kick(&mut h.zone, alice);
    h.tick(10);
    assert!(h.zone.players.get(alice).is_none());
    h.tick(400);
    assert!(h.zone.arenas.find("0").is_none(), "empty arena reaped");
}

#[test]
fn hostile_packets_are_ignored() {
    let mut h = H::new();
    let p = h.connect();
    for bad in [
        vec![c2s::kind::LOGIN],
        vec![c2s::kind::LOGIN; 101],
        vec![c2s::kind::GO_ARENA; 26],
        vec![c2s::kind::CHAT, 2, 0, 0, 0, 0],
        vec![c2s::kind::CHAT; 600],
        vec![c2s::kind::LEAVE_ARENA, 1],
        vec![0xFF, 1, 2],
    ] {
        h.send(p, &bad);
        h.tick(2);
    }
    // a login with a name of only invalid characters is refused
    let mut buf = Vec::new();
    login_packet("::::", "x").encode(&mut buf);
    let p2 = h.connect();
    h.send(p2, &buf);
    h.tick(3);
    let lr = h
        .take(p2)
        .into_iter()
        .find(|d| d[0] == s2c::kind::LOGIN_RESPONSE)
        .expect("lr");
    assert_eq!(
        LoginResponse::decode(&lr).expect("lr").code,
        auth_code::BAD_NAME
    );
}

#[test]
fn ships_and_positions() {
    use oss_proto::position::{C2SPosition, S2CPosition, S2CWeapon, WeaponInfo, weapon_type};
    let mut h = H::new();
    let (alice, _) = h.login("alice", "x");
    let (bob, _) = h.login("bob", "secret");
    h.enter(alice);
    h.enter(bob);
    h.take(alice);

    // alice leaves spec: balanced freq 0, everyone is told
    h.send(alice, &[c2s::kind::SET_SHIP, 0]);
    let mine = h.take(alice);
    let theirs = h.take(bob);
    for pk in [&mine, &theirs] {
        let sc = pk
            .iter()
            .find(|d| d[0] == s2c::kind::SHIP_CHANGE)
            .expect("ship change");
        let sc = s2c::ShipChange::decode(sc).expect("decode");
        assert_eq!((sc.ship, sc.pid, sc.freq), (0, alice.0 as i16, 0));
    }
    assert_eq!(
        h.zone.players.get(alice).map(|p| (p.ship.0, p.freq)),
        Some((0, 0))
    );
    // bad ship numbers are ignored
    h.send(alice, &[c2s::kind::SET_SHIP, 9]);
    assert!(h.take(bob).is_empty());

    // a plain position near bob (who sits at 0,0 in spec) is relayed as 0x28
    let mut pos = C2SPosition {
        rotation: 10,
        time: h.t as u32,
        x: 400,
        y: 300,
        bounty: 7,
        energy: 1000,
        ..Default::default()
    };
    let mut buf = Vec::new();
    pos.encode(&mut buf);
    assert_eq!(buf.len(), 22);
    h.send(alice, &buf);
    let got = h.take(bob);
    let p28 = got.iter().find(|d| d[0] == 0x28).expect("position relayed");
    let sp = S2CPosition::decode(p28).expect("decode");
    assert_eq!(
        (sp.x, sp.y, sp.player_id, sp.bounty),
        (400, 300, alice.0 as u8, 7)
    );
    assert!(h.take(alice).is_empty(), "no echo to the sender");
    assert_eq!(
        h.zone.players.get(alice).map(|p| (p.pos.x, p.pos.y)),
        Some((400, 300))
    );

    // a corrupted checksum is dropped
    buf[10] ^= 0x55;
    h.send(alice, &buf);
    assert!(h.take(bob).is_empty());

    // a bullet is relayed as 0x05 with a valid checksum
    pos.time += 1;
    let mut w = WeaponInfo::default();
    w.set_kind(weapon_type::BULLET);
    pos.weapon = w;
    buf.clear();
    pos.encode(&mut buf);
    h.send(alice, &buf);
    let got = h.take(bob);
    let p05 = got.iter().find(|d| d[0] == 0x05).expect("weapon relayed");
    let sw = S2CWeapon::decode(p05).expect("decode");
    assert!(sw.checksum_ok());
    assert_eq!(
        (sw.player_id, sw.weapon.kind()),
        (alice.0, weapon_type::BULLET)
    );

    // far away and not a weapon: only an occasional radar update, never in range
    pos.time += 1;
    pos.weapon = WeaponInfo::default();
    pos.x = 16000;
    pos.y = 16000;
    buf.clear();
    pos.encode(&mut buf);
    h.send(alice, &buf);
    assert!(h.take(bob).iter().all(|d| d[0] != 0x28));

    // spectators' positions are never relayed
    let spec_pos = C2SPosition {
        time: h.t as u32,
        x: 1,
        y: 1,
        ..Default::default()
    };
    buf.clear();
    spec_pos.encode(&mut buf);
    h.send(bob, &buf);
    assert!(h.take(alice).is_empty());

    // back to spec via the spectator frequency
    let mut sf = Vec::new();
    c2s::SetFreq { freq: 8025 }.encode(&mut sf);
    h.send(alice, &sf);
    assert_eq!(
        h.zone.players.get(alice).map(|p| (p.ship.0, p.freq)),
        Some((8, 8025))
    );
}

#[test]
fn map_download_without_a_zone_dir_uses_tinymap() {
    let mut h = H::new();
    let (p, _) = h.login("mapper", "x");
    let pkts = h.enter(p);
    let mf = pkts
        .iter()
        .find(|d| d[0] == s2c::kind::MAP_FILENAME)
        .expect("0x29");
    assert_eq!(mf.len(), 21);
    match s2c::MapFilename::decode(mf).expect("decode") {
        s2c::MapFilename::Single { name, checksum } => {
            assert_eq!(name.as_bytes(), b"tinymap.lvl");
            assert_eq!(checksum, 0x5643_ef8a);
        }
        other => panic!("{other:?}"),
    }
    h.send(p, &[c2s::kind::MAP_REQUEST]);
    let got = h.take(p);
    let md = got
        .iter()
        .find(|d| d[0] == s2c::kind::MAP_DATA)
        .expect("0x2A");
    assert_eq!(md.len(), 29);
    // bad lengths and unknown LVZ indexes are ignored
    h.send(p, &[c2s::kind::MAP_REQUEST, 5]);
    h.send(p, &[c2s::kind::MAP_REQUEST, 5, 0]);
    assert!(h.take(p).iter().all(|d| d[0] != s2c::kind::MAP_DATA));
}

#[test]
fn simultaneous_entries_are_announced_once() {
    let mut h = H::new();
    let ids: Vec<PlayerId> = ["p1", "p2", "p3"]
        .iter()
        .map(|n| h.login(n, "x").0)
        .collect();
    // all three ask in the same tick
    for &p in &ids {
        let go = GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            ..Default::default()
        };
        let mut buf = Vec::new();
        go.encode(&mut buf);
        h.send(p, &buf);
    }
    h.tick(10);
    for &p in &ids {
        let mut names: Vec<Vec<u8>> = h
            .take(p)
            .iter()
            .filter(|d| d[0] == s2c::kind::PLAYER_ENTERING)
            .map(|d| {
                s2c::PlayerEntering::decode(d).expect("0x03").players[0]
                    .name
                    .as_bytes()
                    .to_vec()
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![b"p1".to_vec(), b"p2".to_vec(), b"p3".to_vec()],
            "for {p:?}"
        );
    }
}

#[test]
fn flood_obscenity_and_restricted_chat() {
    let global = "[Chat]\nFloodLimit = 3\nFloodShutup = 60\nObscene = darn ?urpx\n";
    let mut h = H::with_conf(ARENA_CONF, global);
    let (alice, _) = h.login("alice", "x");
    let (bob, _) = h.login("bob", "secret");
    h.enter(alice);
    h.enter(bob);
    h.take(alice);
    if let Some(p) = h.zone.players.get_mut(bob) {
        p.obscenity_filter = true;
    }

    // bob (filter on) gets the word replaced; rot-13 entries work too
    h.chat(alice, chat_type::PUBLIC, 0, "well DARN, heck");
    let got = chats(&h.take(bob));
    let text = String::from_utf8_lossy(&got[0].text).into_owned();
    assert!(
        !text.to_ascii_lowercase().contains("darn") && !text.contains("heck"),
        "{text}"
    );
    assert!(text.starts_with("well ") && text.len() == "well DARN, heck".len());

    // the third message inside a second trips flood control; the fourth is dropped
    h.chat(alice, chat_type::PUBLIC, 0, "two");
    assert!(chats(&h.take(alice)).is_empty());
    h.chat(alice, chat_type::PUBLIC, 0, "three");
    let got = chats(&h.take(alice));
    assert!(
        String::from_utf8_lossy(&got[0].text).contains("shut up for 60 seconds"),
        "{got:?}"
    );
    h.take(bob);
    h.chat(alice, chat_type::PUBLIC, 0, "four");
    assert!(chats(&h.take(bob)).is_empty());
    // commands are still allowed while shut up (they count toward flooding too)
    h.chat(alice, chat_type::PUBLIC, 0, "?version");
    let got = chats(&h.take(alice));
    assert!(
        got.iter()
            .any(|c| String::from_utf8_lossy(&c.text).contains("Open SubSpace"))
    );
}

#[test]
fn arena_chat_restrictions() {
    // RestrictChat bit 2 = public messages
    let conf = format!("{ARENA_CONF}[Chat]\nRestrictChat = 4\n");
    let mut h = H::with_conf(&conf, "[Chat]\n");
    let (alice, _) = h.login("alice", "x");
    let (bob, _) = h.login("bob", "secret");
    h.enter(alice);
    h.enter(bob);
    h.take(bob);
    h.chat(alice, chat_type::PUBLIC, 0, "hello");
    assert!(chats(&h.take(bob)).is_empty());
    h.chat(alice, chat_type::PRIVATE, bob.0 as i16, "psst");
    assert_eq!(chats(&h.take(bob)).len(), 1);
}

#[test]
fn staff_commands() {
    let mut h = H::new();
    let (alice, _) = h.login("alice", "x");
    let (bob, _) = h.login("bob", "secret");
    h.enter(alice);
    h.enter(bob);
    h.take(alice);
    h.take(bob);
    let arena_id = h
        .zone
        .players
        .get(bob)
        .and_then(|p| p.arena)
        .expect("arena");
    let _ = arena_id;

    // ?setship aimed at alice (sent as a private message)
    h.chat(bob, chat_type::PRIVATE, alice.0 as i16, "?setship 3");
    assert_eq!(h.zone.players.get(alice).map(|p| p.ship.0), Some(2));
    // alice (default group) can't do that to bob
    h.chat(alice, chat_type::PRIVATE, bob.0 as i16, "?setship 1");
    assert_eq!(h.zone.players.get(bob).map(|p| p.ship.0), Some(8));

    // ?lock -s: alice goes to spec and can't leave it
    h.chat(bob, chat_type::PRIVATE, alice.0 as i16, "?lock -s");
    assert_eq!(h.zone.players.get(alice).map(|p| p.ship.0), Some(8));
    h.take(alice);
    h.send(alice, &[c2s::kind::SET_SHIP, 0]);
    assert_eq!(h.zone.players.get(alice).map(|p| p.ship.0), Some(8));
    assert!(
        chats(&h.take(alice))
            .iter()
            .any(|c| String::from_utf8_lossy(&c.text).contains("locked"))
    );

    // ?prize and ?warpto send packets to the target
    h.chat(bob, chat_type::PRIVATE, alice.0 as i16, "?prize 2 #13");
    let pr = h
        .take(alice)
        .into_iter()
        .find(|d| d[0] == s2c::kind::PRIZE_RECEIVE)
        .expect("0x20");
    let pr = s2c::PrizeReceive::decode(&pr).expect("decode");
    assert_eq!((pr.count, pr.prize), (2, 13));
    h.chat(bob, chat_type::PRIVATE, alice.0 as i16, "?warpto 512 300");
    let w = h
        .take(alice)
        .into_iter()
        .find(|d| d[0] == s2c::kind::WARP_TO)
        .expect("0x32");
    assert_eq!(
        s2c::WarpTo::decode(&w).map(|w| (w.x, w.y)).expect("decode"),
        (512, 300)
    );

    // ?arena lists our arena with a negative population
    h.take(bob);
    h.chat(bob, chat_type::PUBLIC, 0, "?arena");
    let al = h
        .take(bob)
        .into_iter()
        .find(|d| d[0] == s2c::kind::ARENA_LIST)
        .expect("0x2F");
    let al = s2c::ArenaList::decode(&al).expect("decode");
    assert_eq!(al.arenas.len(), 1);
    assert_eq!(
        (al.arenas[0].name.as_slice(), al.arenas[0].population),
        (b"0".as_slice(), -2)
    );

    // ?z reaches everyone
    h.take(alice);
    h.chat(bob, chat_type::PUBLIC, 0, "?z server restart soon");
    let got: Vec<String> = chats(&h.take(alice))
        .iter()
        .map(|c| String::from_utf8_lossy(&c.text).into_owned())
        .collect();
    assert!(
        got.iter()
            .any(|t| t.starts_with("server restart soon  -bob")),
        "{got:?} {:?}",
        chats(&h.take(bob))
    );
}

fn texts(pkts: &[Vec<u8>]) -> Vec<String> {
    chats(pkts)
        .iter()
        .map(|c| String::from_utf8_lossy(&c.text).into_owned())
        .collect()
}

#[test]
fn banners_reach_the_arena() {
    let mut h = H::new();
    let (alice, _) = h.login("alice", "x");
    let (bob, _) = h.login("bob", "secret");
    h.enter(alice);
    h.enter(bob);
    h.take(alice);
    h.take(bob);

    let mut pkt = vec![c2s::kind::BANNER];
    pkt.extend((0..96).map(|i| i as u8));
    h.send(alice, &pkt);
    let b = h
        .take(bob)
        .into_iter()
        .find(|d| d[0] == s2c::kind::BANNER)
        .expect("0x1F");
    let b = s2c::Banner::decode(&b).expect("decode");
    assert_eq!(b.pid, alice.0 as i16);
    assert_eq!(b.banner.0[95], 95);

    // in a ship: refused
    h.send(bob, &[c2s::kind::SET_SHIP, 0]);
    h.take(bob);
    h.send(bob, &pkt);
    let out = h.take(bob);
    assert!(!out.iter().any(|d| d[0] == s2c::kind::BANNER));
    assert!(texts(&out).iter().any(|t| t.contains("spectator mode")));
    // short packets are ignored
    h.send(alice, &pkt[..50]);

    // late arrivals get the banners already shown
    let (carol, _) = h.login("carol", "x");
    let out = h.enter(carol);
    assert!(
        out.iter()
            .filter(|d| d[0] == s2c::kind::BANNER)
            .filter_map(|d| s2c::Banner::decode(d).ok())
            .any(|b| b.pid == alice.0 as i16)
    );
}

#[test]
fn freq_owners() {
    let mut h = H::new();
    let (alice, _) = h.login("alice", "x");
    let (bob, _) = h.login("bob", "secret");
    let (carol, _) = h.login("carol", "x");
    for p in [alice, bob, carol] {
        h.enter(p);
        h.send(p, &[c2s::kind::SET_SHIP, 0]);
    }
    let freq = |h: &H, p: PlayerId| h.zone.players.get(p).map(|x| x.freq);
    h.take(alice);
    h.send(alice, &[c2s::kind::SET_FREQ, 100, 0]);
    assert_eq!(freq(&h, alice), Some(100));
    assert!(
        texts(&h.take(alice))
            .iter()
            .any(|t| t.contains("owner of freq 100"))
    );
    h.take(bob);
    h.send(bob, &[c2s::kind::SET_FREQ, 100, 0]);
    assert!(
        texts(&h.take(bob))
            .iter()
            .any(|t| t.contains("This freq has an owner"))
    );

    // a non-owner can't kick; the owner can, after a delay
    h.chat(bob, chat_type::PRIVATE, alice.0 as i16, "?freqkick");
    assert!(
        texts(&h.take(bob))
            .iter()
            .any(|t| t == "You are not the owner of your freq.")
    );
    h.chat(alice, chat_type::PRIVATE, bob.0 as i16, "?freqkick");
    assert_eq!(freq(&h, bob), Some(100));
    // a ship change on the same freq doesn't dodge the kick
    h.send(bob, &[c2s::kind::SET_SHIP, 1]);
    h.tick(301);
    assert_eq!(h.zone.players.get(bob).map(|p| p.ship.0), Some(8));
    assert!(
        texts(&h.take(bob))
            .iter()
            .any(|t| t == "You have been kicked off your freq.")
    );

    // ownership can be shared, and claimed once the owners leave
    h.send(carol, &[c2s::kind::SET_FREQ, 100, 0]);
    h.chat(alice, chat_type::PRIVATE, carol.0 as i16, "?giveowner");
    h.chat(carol, chat_type::PRIVATE, alice.0 as i16, "?freqkick");
    assert!(
        texts(&h.take(carol))
            .iter()
            .any(|t| t == "You can not kick other freq owners off your freq.")
    );
    h.send(alice, &[c2s::kind::SET_SHIP, 1]);
    assert!(oss_server::modules::freqowners::owns_freq(&h.zone, alice));
    h.send(bob, &[c2s::kind::SET_SHIP, 0]);
    h.send(bob, &[c2s::kind::SET_FREQ, 100, 0]);
    h.send(alice, &[c2s::kind::SET_FREQ, 5, 0]);
    h.send(carol, &[c2s::kind::SET_FREQ, 6, 0]);
    h.take(bob);
    h.chat(bob, chat_type::PUBLIC, 0, "?takeownership");
    assert!(
        texts(&h.take(bob))
            .iter()
            .any(|t| t == "You are now the owner of your freq.")
    );
    h.send(alice, &[c2s::kind::SET_FREQ, 100, 0]);
    h.chat(alice, chat_type::PUBLIC, 0, "?takeownership");
    let out = texts(&h.take(alice));
    assert!(
        out.iter()
            .any(|t| t == "Your freq already has one or more owners:")
    );
    assert!(out.iter().any(|t| t == "bob"));
    // public freqs never have owners
    h.send(carol, &[c2s::kind::SET_FREQ, 6, 0]);
    h.take(carol);
    h.chat(carol, chat_type::PUBLIC, 0, "?takeownership");
    assert!(
        texts(&h.take(carol))
            .iter()
            .any(|t| t == "Your freq can not have an owner.")
    );
}

#[test]
fn redirects() {
    use oss_server::modules::redirect::Redirect;
    let mut h = H::with_conf(
        ARENA_CONF,
        "[Chat]\nMessageReliable = 1\n[Redirects]\nother = 127.0.0.1:5100:duel\n",
    );
    let (alice, _) = h.login("alice", "x");
    h.enter(alice);
    h.take(alice);
    let r = h.zone.iface::<dyn Redirect>().expect("redirect");
    assert!(r.alias_redirect(&mut h.zone, &[alice], "other"));
    assert!(r.alias_redirect(&mut h.zone, &[alice], "10.1.2.3:6000"));
    assert!(!r.alias_redirect(&mut h.zone, &[alice], "nowhere"));
    assert!(r.arena_request(&mut h.zone, alice, "other"));
    h.lo.collect(&mut h.zone);
    let got: Vec<_> = h
        .take(alice)
        .into_iter()
        .filter(|d| d[0] == s2c::kind::REDIRECT)
        .filter_map(|d| s2c::Redirect::decode(&d).ok())
        .map(|r| {
            (
                r.ip,
                r.port,
                r.arena_type,
                r.arena.to_str().map(str::to_owned),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ([127, 0, 0, 1], 5100, -3, Some("duel".to_owned())),
            ([10, 1, 2, 3], 6000, -1, Some(String::new())),
            ([127, 0, 0, 1], 5100, -3, Some("duel".to_owned())),
        ]
    );
}
