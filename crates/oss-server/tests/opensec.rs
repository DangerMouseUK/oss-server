//! OpenSec end to end over real UDP: a pinned client connects and plays,
//! a wrongly pinned client is refused, VIE still works beside it, and the
//! server key survives a restart.

use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use oss_net::opensec::{OpenSecClientHandshake, os_entropy};
use oss_net::{Config, Connection, Event, Handshake, VieClientHandshake};
use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::s2c;
use oss_proto::wire::FixedStr;

use oss_server::opensec::ServerPublicKey;
use oss_server::runtime::{RunningServer, ServerBuilder};

struct Client {
    sock: UdpSocket,
    server: SocketAddr,
    conn: Connection,
    start: Instant,
    inbox: Vec<Vec<u8>>,
    connected: bool,
    failed: bool,
}

impl Client {
    fn new(server: SocketAddr, hs: Box<dyn Handshake>) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        sock.set_read_timeout(Some(Duration::from_millis(5)))
            .expect("timeout");
        Self {
            sock,
            server,
            conn: Connection::client(Config::client(), hs, 0),
            start: Instant::now(),
            inbox: Vec::new(),
            connected: false,
            failed: false,
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
                Event::Disconnected(_) => self.failed = true,
                _ => {}
            }
        }
        while let Some(d) = self.conn.poll_transmit() {
            let _ = self.sock.send_to(&d, self.server);
        }
    }

    fn try_connect(&mut self, secs: u64) -> bool {
        let until = Instant::now() + Duration::from_secs(secs);
        while !self.connected && !self.failed && Instant::now() < until {
            self.pump();
        }
        self.connected
    }

    fn wait_for(&mut self, kind: u8) -> Vec<Vec<u8>> {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if let Some(i) = self.inbox.iter().position(|p| p.first() == Some(&kind)) {
                return self.inbox.drain(..=i).collect();
            }
            self.pump();
        }
        panic!("no packet {kind:#04x}");
    }

    fn send(&mut self, p: &[u8]) {
        self.conn.send(p, true);
        self.pump();
    }

    fn play(&mut self, name: &str) {
        assert_eq!(self.login(name), 0);
        let mut buf = Vec::new();
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

    /// Logs in; returns the login result code.
    fn login(&mut self, name: &str) -> u8 {
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
        self.send(&buf);
        let lr = self.wait_for(s2c::kind::LOGIN_RESPONSE);
        let lr = s2c::LoginResponse::decode(lr.last().expect("lr")).expect("decode");
        lr.code
    }
}

fn start(dir: &std::path::Path) -> RunningServer {
    ServerBuilder::new(dir)
        .bind(([127, 0, 0, 1], 0).into())
        .start()
        .expect("server")
}

fn public_key(server: &RunningServer) -> [u8; 32] {
    let (tx, rx) = mpsc::channel();
    server.handle().run(move |z| {
        let _ = tx.send(z.res.get::<ServerPublicKey>().map(|k| k.0));
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("zone")
        .expect("OpenSec on by default")
}

fn ciphers(server: &RunningServer) -> Vec<(String, Option<&'static str>)> {
    let (tx, rx) = mpsc::channel();
    server.handle().run(move |z| {
        let mut v: Vec<_> = z
            .players
            .iter()
            .map(|p| (p.name.clone(), p.cipher))
            .collect();
        v.sort();
        let _ = tx.send(v);
    });
    rx.recv_timeout(Duration::from_secs(5)).expect("zone")
}

#[test]
fn opensec_end_to_end() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("oss-opensec-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(dir.join("conf")).expect("mkdir");
    std::fs::create_dir_all(dir.join("arenas/(default)")).expect("mkdir");
    std::fs::write(dir.join("conf/global.conf"), "").expect("write");
    std::fs::write(
        dir.join("conf/passwd.conf"),
        "[General]\nAllowUnknown = yes\n",
    )
    .expect("write");
    std::fs::write(dir.join("arenas/(default)/arena.conf"), "").expect("write");

    let server = start(&dir);
    let addr = server.listen_addrs()[0];
    let key = public_key(&server);
    assert!(dir.join("data/opensec.key").exists(), "key saved");

    // pinned OpenSec client
    let mut good = Client::new(
        addr,
        Box::new(OpenSecClientHandshake::new(&os_entropy(), Some(key))),
    );
    assert!(good.try_connect(10));
    assert_eq!(good.conn.cipher_name(), Some("opensec"));
    good.play("secure");

    // a VIE client next to it
    let mut vie = Client::new(addr, Box::new(VieClientHandshake::new(3)));
    assert!(vie.try_connect(10));
    vie.play("legacy");
    assert_eq!(
        ciphers(&server),
        vec![
            ("legacy".to_owned(), Some("vie")),
            ("secure".to_owned(), Some("opensec")),
        ]
    );

    // a client pinning some other key never gets a session
    let mut wrong = Client::new(
        addr,
        Box::new(OpenSecClientHandshake::new(&os_entropy(), Some([7; 32]))),
    );
    assert!(!wrong.try_connect(2));
    drop((good, vie, wrong));
    assert!(server.stop().is_some());

    // the key survives a restart
    let again = start(&dir);
    assert_eq!(public_key(&again), key);
    assert!(again.stop().is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn identity_keys_and_key_bans() {
    use oss_net::opensec::IdentityKey;
    use oss_server::modules::bans::{self, Ban};
    let src = oss_server::config::MemSource::new([
        ("conf/global.conf", ""),
        (
            "conf/passwd.conf",
            "[General]
AllowUnknown = yes
",
        ),
        ("arenas/(default)/arena.conf", ""),
    ]);
    let server = ServerBuilder::with_source(src)
        .bind(([127, 0, 0, 1], 0).into())
        .start()
        .expect("server");
    let addr = server.listen_addrs()[0];
    let key = public_key(&server);
    let me = IdentityKey::generate(&os_entropy());
    let connect = |id: &IdentityKey| {
        let mut c = Client::new(
            addr,
            Box::new(
                OpenSecClientHandshake::new(&os_entropy(), Some(key)).with_identity(id.clone()),
            ),
        );
        assert!(c.try_connect(10));
        c
    };

    // the server learns the identity with the login
    let mut a = connect(&me);
    a.play("keyed");
    let (tx, rx) = mpsc::channel();
    server.handle().run(move |z| {
        let k = z.players.find_by_name("keyed").and_then(|p| p.identity_key);
        let _ = tx.send(k);
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).expect("zone"),
        Some(me.public())
    );

    // a ban on the key holds under any name; another key gets in
    let public = me.public();
    server.handle().run(move |z| {
        bans::add(
            z,
            &Ban {
                id: 0,
                name: None,
                ip: None,
                machine_id: None,
                key: Some(public),
                expires: None,
                reason: "test".into(),
                by: "test".into(),
            },
        );
    });
    drop(a);
    let mut b = connect(&me);
    assert_eq!(
        b.login("renamed"),
        oss_server::modules::core::auth_code::LOCKED_OUT
    );
    let mut c = connect(&IdentityKey::generate(&os_entropy()));
    assert_eq!(c.login("someone"), 0);
    drop((b, c));
    assert!(server.stop().is_some());
}
