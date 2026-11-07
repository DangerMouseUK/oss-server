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
        let passwd = format!(
            "[General]\nAllowUnknown = yes\n[Users]\nbob = {}\nlocked = lock\n",
            hash_password("Bob", "secret")
        );
        let src = MemSource::new([
            (
                "conf/global.conf",
                "[Chat]\nMessageReliable = 1\n".to_owned(),
            ),
            ("conf/passwd.conf", passwd),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_version = 1\ncmd_go = 1\ncmd_help = 1\n\
                 [mod]\ncmd_version = 1\ncmd_go = 1\nseemodchat\nsendmodchat\n"
                    .to_owned(),
            ),
            ("conf/staff.conf", "[(global)]\nbob = mod\n".to_owned()),
            ("arenas/(default)/arena.conf", ARENA_CONF.to_owned()),
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
            s2c::kind::BRICK,
            s2c::kind::ENTERING_ARENA
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
