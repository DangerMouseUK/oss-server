//! The chat protocol over real TCP, next to a game-protocol player on the
//! loopback transport.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::{Zone, ZoneStarted};

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
                "[Net]\nChatListen = 127.0.0.1:0\nChatMessageDelay = 0\n",
            ),
            ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
            ("conf/groupdef.conf", "[default]\ncmd_go = 1\n"),
            ("conf/staff.conf", ""),
            ("arenas/(default)/arena.conf", "[Team]\nDesiredTeams = 2\n"),
        ]);
        let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        modules.push("chatnet".into());
        let mut zone = ServerBuilder::with_source(src)
            .modules(modules)
            .build_zone()
            .expect("zone");
        zone.fire(&ZoneStarted);
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

    fn game_player(&mut self, name: &str) -> PlayerId {
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
        self.zone.handle_packet(p, &buf);
        for _ in 0..10 {
            self.tick();
        }
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
        for _ in 0..10 {
            self.tick();
        }
        p
    }

    fn game_chats(&mut self, p: PlayerId) -> Vec<(u8, i16, String)> {
        self.lo
            .take(p)
            .into_iter()
            .filter(|(d, _)| d[0] == s2c::kind::CHAT)
            .map(|(d, _)| {
                let c = S2CChat::decode(&d).expect("chat").0;
                (c.kind, c.pid, String::from_utf8_lossy(&c.text).into_owned())
            })
            .collect()
    }
}

struct Client {
    w: TcpStream,
    r: BufReader<TcpStream>,
    seen: Vec<String>,
}

impl Client {
    fn connect(h: &H) -> Self {
        let addr = oss_server::modules::chatnet::local_addr(&h.zone).expect("listening");
        let s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_millis(2)))
            .expect("timeout");
        Self {
            r: BufReader::new(s.try_clone().expect("clone")),
            w: s,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, line: &str) {
        self.w
            .write_all(format!("{line}\r\n").as_bytes())
            .expect("write");
    }

    fn poll(&mut self) {
        let mut l = String::new();
        while let Ok(n) = self.r.read_line(&mut l) {
            if n == 0 || !l.ends_with('\n') {
                break;
            }
            self.seen.push(l.trim_end().to_owned());
            l.clear();
        }
    }
}

/// Ticks the zone until `want` is among the lines `c` received.
fn expect(h: &mut H, c: &mut Client, want: &str) {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        h.tick();
        c.poll();
        if let Some(i) = c.seen.iter().position(|l| l == want) {
            c.seen.drain(..=i);
            return;
        }
    }
    panic!("never got {want:?}; got {:?}", c.seen);
}

fn settle(h: &mut H, cs: &mut [&mut Client]) {
    for _ in 0..20 {
        h.tick();
        for c in cs.iter_mut() {
            c.poll();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn chat_protocol() {
    let mut h = H::new();
    let alice = h.game_player("alice");

    let mut a = Client::connect(&h);
    a.send("LOGIN:1;testbot:chatter:pw");
    expect(&mut h, &mut a, "LOGINOK:chatter");
    a.send("GO:");
    expect(&mut h, &mut a, "INARENA:0:8025");
    expect(&mut h, &mut a, "PLAYER:alice:8:8025");
    // the game client hears of the chat client like any player
    let chatter = h
        .zone
        .players
        .find_by_name("chatter")
        .map(|p| p.id)
        .expect("chatter");
    assert_eq!(
        h.zone.players.get(chatter).map(|p| p.client_name.clone()),
        Some("chat: testbot".to_owned())
    );

    // chat both ways
    h.lo.take(alice);
    a.send("SEND:PUB:hi there");
    settle(&mut h, &mut [&mut a]);
    assert!(h.game_chats(alice).contains(&(
        chat_type::PUBLIC,
        chatter.0 as i16,
        "hi there".to_owned()
    )));
    let mut buf = Vec::new();
    c2s::C2SChat(ChatBody {
        kind: chat_type::PUBLIC,
        sound: 0,
        pid: 0,
        text: b"hello".to_vec(),
    })
    .encode(&mut buf);
    h.zone.handle_packet(alice, &buf);
    expect(&mut h, &mut a, "MSG:PUB:alice:hello");

    // a second chat client: entering, private messages, freq changes, leaving
    let mut b = Client::connect(&h);
    b.send("LOGIN:1:bob2:pw");
    expect(&mut h, &mut b, "LOGINOK:bob2");
    b.send("GO:");
    expect(&mut h, &mut a, "ENTERING:bob2:8:8025");
    expect(&mut h, &mut b, "PLAYER:chatter:8:8025");
    a.send("SEND:PRIV:bob2:psst");
    expect(&mut h, &mut b, "MSG:PRIV:chatter:psst");
    a.send("SEND:PRIV:nobody:hello?");
    b.send("CHANGEFREQ:3");
    expect(&mut h, &mut a, "SHIPFREQCHANGE:bob2:8:3");
    h.zone.handle_packet(alice, &[c2s::kind::SET_SHIP, 0]);
    let line = |h: &H| {
        let f = h.zone.players.get(alice).map_or(0, |p| p.freq);
        format!("SHIPFREQCHANGE:alice:0:{f}")
    };
    let want = line(&h);
    expect(&mut h, &mut b, &want);
    // commands answer as arena messages
    a.send("SEND:CMD:arena");
    settle(&mut h, &mut [&mut a]);
    drop(b);
    expect(&mut h, &mut a, "LEAVING:bob2");

    // a bad login
    let mut c = Client::connect(&h);
    c.send("LOGIN:1:%%%");
    settle(&mut h, &mut [&mut c]);
    c.send("NOOP");
    settle(&mut h, &mut [&mut c]);
    assert!(
        c.seen.iter().all(|l| !l.starts_with("LOGINOK")),
        "{:?}",
        c.seen
    );
}
