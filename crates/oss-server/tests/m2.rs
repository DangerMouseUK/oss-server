//! Milestone M2 "Hello zone", in process: a real `oss-server` on a UDP port
//! and two clients built on `oss-net`'s client role, talking VIE-encrypted
//! SubSpace protocol over loopback. Each client connects, logs in (password
//! file), enters an arena, receives the settings (byte-identical to the
//! clientset builder) and the map name, downloads the map and the news, and
//! the two exchange positions and chat. The ping port answers too.

use std::collections::VecDeque;
use std::io::Read;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use oss_net::{Config, Connection, Event, VieClientHandshake};
use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::position::C2SPosition;
use oss_proto::s2c::{self, ChatBody, LoginResponse, MapFilename, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::{ConfigManager, ZoneDir};
use oss_server::modules::auth_file::hash_password;
use oss_server::runtime::ServerBuilder;

struct TempZone(PathBuf);

impl Drop for TempZone {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A tiny synthetic map: a border of tiles plus a few blocks (plain tile list).
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

fn make_zone() -> TempZone {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let root = std::env::temp_dir().join(format!("oss-m2-{}-{nanos}", std::process::id()));
    let w = |rel: &str, data: &[u8]| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, data).expect("write");
    };
    w("conf/global.conf", b"[General]\nNewsFile = news.txt\n");
    w(
        "conf/passwd.conf",
        format!(
            "[General]\nAllowUnknown = yes\n[Users]\nalice = {}\n",
            hash_password("alice", "pw")
        )
        .as_bytes(),
    );
    w("conf/groupdef.conf", b"[default]\ncmd_go\ncmd_version\n");
    w("conf/staff.conf", b"");
    w(
        "arenas/(default)/arena.conf",
        b"[General]\nMap = test.lvl\n[Warbird]\nInitialSpeed = 2000\nMaximumSpeed = 6000\n\
          [Misc]\nBounceFactor = 22\n[Team]\nMaxFrequency = 100\n",
    );
    w("maps/test.lvl", &synthetic_map());
    w("news.txt", b"Welcome to the test zone!");
    TempZone(root)
}

struct Client {
    sock: UdpSocket,
    server: SocketAddr,
    conn: Connection,
    start: Instant,
    inbox: VecDeque<Vec<u8>>,
    connected: bool,
}

impl Client {
    fn new(server: SocketAddr, seed: u32) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        sock.set_read_timeout(Some(Duration::from_millis(5)))
            .expect("timeout");
        let conn = Connection::client(Config::client(), Box::new(VieClientHandshake::new(seed)), 0);
        Self {
            sock,
            server,
            conn,
            start: Instant::now(),
            inbox: VecDeque::new(),
            connected: false,
        }
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn pump(&mut self) {
        let now = self.now();
        self.conn.tick(now);
        while let Some(d) = self.conn.poll_transmit() {
            let _ = self.sock.send_to(&d, self.server);
        }
        let mut buf = [0u8; 2048];
        while let Ok((n, _)) = self.sock.recv_from(&mut buf) {
            let now = self.now();
            self.conn.handle_datagram(now, &buf[..n]);
        }
        while let Some(e) = self.conn.poll_event() {
            match e {
                Event::Connected => self.connected = true,
                Event::Packet(p) => self.inbox.push_back(p),
                Event::Disconnected(r) => panic!("disconnected: {r:?}"),
                _ => {}
            }
        }
        while let Some(d) = self.conn.poll_transmit() {
            let _ = self.sock.send_to(&d, self.server);
        }
    }

    fn send(&mut self, p: &[u8], reliable: bool) {
        self.conn.send(p, reliable);
        self.pump();
    }

    /// Pumps until a packet of type `kind` arrives; returns it, keeping the
    /// packets received before it in `seen`.
    fn wait_for(&mut self, kind: u8, seen: &mut Vec<Vec<u8>>) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            while let Some(p) = self.inbox.pop_front() {
                if p.first() == Some(&kind) {
                    return p;
                }
                seen.push(p);
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for packet {kind:#04x}; got {:?}",
                seen.iter().map(|p| p[0]).collect::<Vec<_>>()
            );
            self.pump();
        }
    }

    fn connect(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.connected {
            assert!(Instant::now() < deadline, "handshake timed out");
            self.pump();
        }
        assert_eq!(self.conn.cipher_name(), Some("vie"));
    }

    fn login(&mut self, name: &str, pw: &str) -> LoginResponse {
        let l = Login {
            kind: c2s::kind::LOGIN,
            flags: 0,
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(pw.as_bytes()),
            machine_id: 7,
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
        l.encode(&mut buf);
        self.send(&buf, true);
        let lr = self.wait_for(s2c::kind::LOGIN_RESPONSE, &mut Vec::new());
        LoginResponse::decode(&lr).expect("login response")
    }

    /// Enters a public arena; returns every packet up to the entry marker.
    fn enter(&mut self) -> Vec<Vec<u8>> {
        let go = GoArena {
            ship: 8,
            allow_audio: 0,
            obscenity_filter: 0,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            arena: FixedStr::new(b""),
            optional_graphics: None,
        };
        let mut buf = Vec::new();
        go.encode(&mut buf);
        self.send(&buf, true);
        let mut seen = Vec::new();
        let last = self.wait_for(s2c::kind::ENTERING_ARENA, &mut seen);
        seen.push(last);
        seen
    }
}

fn inflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .expect("zlib");
    out
}

#[test]
fn hello_zone() {
    let zone = make_zone();
    let server = ServerBuilder::new(&zone.0)
        .bind("127.0.0.1:0".parse().expect("addr"))
        .start()
        .expect("server starts");
    let addr = server.game_addr();

    // --- alice: connect, log in with her password, enter, get settings + map
    let mut alice = Client::new(addr, 1);
    alice.connect();
    let lr = alice.login("alice", "pw");
    assert_eq!(lr.code, 0);
    let news = std::fs::read(zone.0.join("news.txt")).expect("news");
    assert_eq!(lr.news_checksum, oss_proto::checksum::crc32(&news));

    let pkts = alice.enter();
    let kinds: Vec<u8> = pkts.iter().map(|p| p[0]).collect();
    assert_eq!(
        kinds,
        vec![
            s2c::kind::PLAYER_ID,
            s2c::kind::SETTINGS,
            s2c::kind::PLAYER_ENTERING,
            s2c::kind::MAP_FILENAME,
            s2c::kind::BRICK,
            s2c::kind::ENTERING_ARENA
        ]
    );
    let mut cfg = ConfigManager::new(ZoneDir::new(&zone.0)).expect("cfg");
    let want = oss_server::clientset::load(&cfg.open_arena("(public)").expect("arena")).block;
    assert_eq!(
        pkts[1].as_slice(),
        want.as_bytes().as_slice(),
        "settings byte-identical"
    );
    let settings = oss_proto::ClientSettings::decode(&pkts[1]).expect("client decodes settings");
    assert_eq!(settings.to_bytes(), pkts[1]);

    let map = std::fs::read(zone.0.join("maps/test.lvl")).expect("map");
    match MapFilename::decode(&pkts[3]).expect("0x29") {
        MapFilename::Single { name, checksum } => {
            assert_eq!(name.as_bytes(), b"test.lvl");
            assert_eq!(checksum, oss_proto::checksum::crc32(&map));
        }
        other => panic!("unexpected {other:?}"),
    }
    alice.send(&[c2s::kind::MAP_REQUEST], true);
    let md = alice.wait_for(s2c::kind::MAP_DATA, &mut Vec::new());
    let md = s2c::MapData::decode(&md).expect("0x2A");
    assert_eq!(md.name.as_bytes(), b"test.lvl");
    assert_eq!(inflate(&md.data), map, "map downloads intact");
    let level = oss_formats::lvl::Level::parse(&inflate(&md.data)).expect("level parses");
    let _ = level;

    alice.send(&[c2s::kind::NEWS_REQUEST], true);
    let nf = alice.wait_for(s2c::kind::INCOMING_FILE, &mut Vec::new());
    assert_eq!(inflate(&nf[17..]), news);

    // --- bob: unknown name allowed; both see each other
    let mut bob = Client::new(addr, 2);
    bob.connect();
    assert_eq!(bob.login("bob", "anything").code, 0);
    let bpkts = bob.enter();
    let entering: Vec<_> = bpkts
        .iter()
        .filter(|p| p[0] == s2c::kind::PLAYER_ENTERING)
        .collect();
    assert_eq!(entering.len(), 2, "bob gets alice and himself");
    let seen = alice.wait_for(s2c::kind::PLAYER_ENTERING, &mut Vec::new());
    let rec = s2c::PlayerEntering::decode(&seen).expect("0x03");
    assert_eq!(rec.players[0].name.as_bytes(), b"bob");
    let alice_id = oss_proto::s2c::PlayerId::decode(&pkts[0])
        .expect("0x01")
        .pid;

    // --- alice takes a ship and flies; bob sees her
    alice.send(&[c2s::kind::SET_SHIP, 0], true);
    let sc = bob.wait_for(s2c::kind::SHIP_CHANGE, &mut Vec::new());
    let sc = s2c::ShipChange::decode(&sc).expect("0x1D");
    assert_eq!((sc.ship, sc.pid), (0, alice_id));
    for i in 0..5i16 {
        let pos = C2SPosition {
            rotation: 0,
            time: alice.conn.server_ticks(alice.now()),
            x: 8000 + i * 10,
            y: 8000,
            x_speed: 100,
            bounty: 3,
            energy: 1500,
            ..Default::default()
        };
        let mut buf = Vec::new();
        pos.encode(&mut buf);
        alice.send(&buf, false);
        std::thread::sleep(Duration::from_millis(15));
    }
    // bob is a spectator at (0,0); the update is beyond his screen but within
    // radar range, so he may get some; make him watch alice to be certain
    let mut spec = Vec::new();
    c2s::SpecRequest { pid: alice_id }.encode(&mut spec);
    bob.send(&spec, true);
    std::thread::sleep(Duration::from_millis(50));
    let pos = C2SPosition {
        time: alice.conn.server_ticks(alice.now()) + 100,
        x: 8100,
        y: 8000,
        ..Default::default()
    };
    let mut buf = Vec::new();
    pos.encode(&mut buf);
    alice.send(&buf, false);
    let p28 = bob.wait_for(0x28, &mut Vec::new());
    let sp = oss_proto::S2CPosition::decode(&p28).expect("0x28");
    assert_eq!(sp.player_id, alice_id as u8);
    assert!(sp.x >= 8000);

    // --- chat both ways
    let mut buf = Vec::new();
    c2s::C2SChat(ChatBody {
        kind: chat_type::PUBLIC,
        sound: 0,
        pid: -1,
        text: b"hi bob".to_vec(),
    })
    .encode(&mut buf);
    alice.send(&buf, true);
    let c = bob.wait_for(s2c::kind::CHAT, &mut Vec::new());
    let c = S2CChat::decode(&c).expect("chat").0;
    assert_eq!(
        (c.kind, c.pid, c.text.as_slice()),
        (chat_type::PUBLIC, alice_id, b"hi bob".as_slice())
    );
    let mut buf = Vec::new();
    c2s::C2SChat(ChatBody {
        kind: chat_type::PRIVATE,
        sound: 0,
        pid: alice_id,
        text: b"hi alice".to_vec(),
    })
    .encode(&mut buf);
    bob.send(&buf, true);
    let c = alice.wait_for(s2c::kind::CHAT, &mut Vec::new());
    assert_eq!(S2CChat::decode(&c).expect("chat").0.text, b"hi alice");
    // a command answered by the server
    let mut buf = Vec::new();
    c2s::C2SChat(ChatBody {
        kind: chat_type::PUBLIC,
        sound: 0,
        pid: -1,
        text: b"?version".to_vec(),
    })
    .encode(&mut buf);
    bob.send(&buf, true);
    let c = bob.wait_for(s2c::kind::CHAT, &mut Vec::new());
    assert_eq!(S2CChat::decode(&c).expect("chat").0.kind, chat_type::ARENA);

    // --- the ping port reports the population
    let ping = UdpSocket::bind("127.0.0.1:0").expect("bind");
    ping.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    ping.send_to(&[1, 2, 3, 4], server.ping_addr())
        .expect("ping");
    let mut buf = [0u8; 64];
    let (n, _) = ping.recv_from(&mut buf).expect("pong");
    assert_eq!(n, 8);
    assert_eq!(&buf[4..8], &[1, 2, 3, 4]);

    // --- bob leaves; alice is told; the server shuts down cleanly
    bob.send(&[c2s::kind::LEAVE_ARENA], true);
    let left = alice.wait_for(s2c::kind::PLAYER_LEAVING, &mut Vec::new());
    assert_eq!(
        s2c::PlayerLeaving::decode(&left).expect("0x04").pid,
        bob_id(&bpkts)
    );
    let zone_state = server.stop().expect("clean stop");
    assert_eq!(zone_state.modules.loaded().count(), 0);
}

fn bob_id(pkts: &[Vec<u8>]) -> i16 {
    oss_proto::s2c::PlayerId::decode(&pkts[0])
        .expect("0x01")
        .pid
}
