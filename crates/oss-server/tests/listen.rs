//! Several listen sockets (ASSS `[Listen]`, `[Listen1]`..): bind addresses,
//! per-socket client types, and `ConnectAs` virtual servers (their arena and
//! their directory entry).

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use oss_net::{Config, Connection, Event, VieClientHandshake};
use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::s2c;
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::runtime::ServerBuilder;

struct Client {
    sock: UdpSocket,
    server: SocketAddr,
    conn: Connection,
    start: Instant,
    inbox: Vec<Vec<u8>>,
    connected: bool,
}

impl Client {
    fn new(server: SocketAddr) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        sock.set_read_timeout(Some(Duration::from_millis(5)))
            .expect("timeout");
        Self {
            sock,
            server,
            conn: Connection::client(Config::client(), Box::new(VieClientHandshake::new(7)), 0),
            start: Instant::now(),
            inbox: Vec::new(),
            connected: false,
        }
    }

    fn pump(&mut self) {
        let now = self.start.elapsed().as_millis() as u64;
        self.conn.tick(now);
        while let Some(d) = self.conn.poll_transmit() {
            let _ = self.sock.send_to(&d, self.server);
        }
        let mut buf = [0u8; 2048];
        while let Ok((n, _)) = self.sock.recv_from(&mut buf) {
            let now = self.start.elapsed().as_millis() as u64;
            self.conn.handle_datagram(now, &buf[..n]);
        }
        while let Some(e) = self.conn.poll_event() {
            match e {
                Event::Connected => self.connected = true,
                Event::Packet(p) => self.inbox.push(p),
                _ => {}
            }
        }
        while let Some(d) = self.conn.poll_transmit() {
            let _ = self.sock.send_to(&d, self.server);
        }
    }

    /// Pumps for up to `secs`; true once connected.
    fn try_connect(&mut self, secs: u64) -> bool {
        let until = Instant::now() + Duration::from_secs(secs);
        while !self.connected && Instant::now() < until {
            self.pump();
        }
        self.connected
    }

    fn wait_for(&mut self, kind: u8) {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if let Some(i) = self.inbox.iter().position(|p| p.first() == Some(&kind)) {
                self.inbox.drain(..=i);
                return;
            }
            self.pump();
        }
        panic!("no packet {kind:#04x}");
    }

    fn send(&mut self, p: &[u8]) {
        self.conn.send(p, true);
        self.pump();
    }

    fn login_and_enter(&mut self, name: &str) {
        let mut buf = Vec::new();
        Login {
            kind: c2s::kind::LOGIN,
            flags: 0,
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(b"x"),
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
        }
        .encode(&mut buf);
        self.send(&buf);
        self.wait_for(s2c::kind::LOGIN_RESPONSE);
        buf.clear();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            ..Default::default()
        }
        .encode(&mut buf);
        self.send(&buf);
        self.wait_for(s2c::kind::ENTERING_ARENA);
    }
}

#[test]
fn listen_sockets() {
    let global = "[Listen]\nPort = 0\nBindAddress = 127.0.0.1\n\
                  [Listen1]\nPort = 0\nBindAddress = 127.0.0.1\nConnectAs = duel\n\
                  [Listen2]\nPort = 0\nBindAddress = 127.0.0.1\nAllowVIE = 0\n";
    let src = MemSource::new([
        ("conf/global.conf", global),
        ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
        ("arenas/(default)/arena.conf", ""),
    ]);
    let server = ServerBuilder::with_source(src).start().expect("server");
    let addrs = server.listen_addrs().to_vec();
    assert_eq!(addrs.len(), 3);
    assert!(addrs.iter().all(|a| a.ip().is_loopback()), "{addrs:?}");

    let mut a = Client::new(addrs[0]);
    assert!(a.try_connect(10));
    a.login_and_enter("alpha");
    let mut b = Client::new(addrs[1]);
    assert!(b.try_connect(10));
    b.login_and_enter("bravo");
    // VIE clients aren't allowed on the third socket: no answer at all
    let mut c = Client::new(addrs[2]);
    assert!(!c.try_connect(1));

    let (tx, rx) = std::sync::mpsc::channel();
    server.handle().run(move |z| {
        let mut where_: Vec<(String, Option<String>, String)> = z
            .players
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    p.connect_as.clone(),
                    p.arena
                        .and_then(|x| z.arenas.get(x))
                        .map(|x| x.name.clone())
                        .unwrap_or_default(),
                )
            })
            .collect();
        where_.sort();
        let _ = tx.send(where_);
    });
    let where_ = rx.recv_timeout(Duration::from_secs(5)).expect("zone");
    assert_eq!(
        where_,
        vec![
            ("alpha".to_owned(), None, "0".to_owned()),
            (
                "bravo".to_owned(),
                Some("duel".to_owned()),
                "duel".to_owned()
            ),
        ]
    );
    assert!(server.stop().is_some());
}

#[test]
fn virtual_server_directory_entry() {
    // the `Virtual` case of PRIVATE's ASSS-compiled directory vectors
    let vectors = include_str!("fixtures/asss-vectors/directory.txt");
    let line = vectors
        .lines()
        .filter(|l| l.starts_with("update "))
        .nth(6)
        .expect("virtual vector");
    let hex = line.split_whitespace().nth(2).expect("hex");
    let mut want: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect();
    // the vector has 9 players on the virtual server; this zone has none
    want[6..8].copy_from_slice(&0u16.to_le_bytes());
    let global = "[Listen]\nPort = 5000\n[Listen1]\nPort = 5002\nConnectAs = vs\n\
                  [Directory]\nName = Main\nPassword = pw\n\
                  [Directory-vs]\nName = Virtual\nDescription = second listen socket\n";
    let src = MemSource::new([
        ("conf/global.conf", global),
        ("arenas/(default)/arena.conf", ""),
    ]);
    let zone = ServerBuilder::with_source(src)
        .modules(["core", "arenaman"])
        .build_zone()
        .expect("zone");
    let pkts = oss_server::modules::directory::packets(&zone);
    assert_eq!(pkts.len(), 2);
    assert_eq!(pkts[1], want);
    // the main socket falls back to [Directory]
    let main = oss_proto::directory::ZoneUpdate::decode(&pkts[0]).expect("decode");
    assert_eq!((main.port, main.name.as_bytes()), (5000, &b"Main"[..]));
}
