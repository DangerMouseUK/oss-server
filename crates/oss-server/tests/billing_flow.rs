//! The `billing_ssc` client: against a scripted biller (exact packets, from
//! PRIVATE's ASSS-compiled vectors where they apply) and end to end against
//! `oss-biller` over UDP with two zones.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oss_proto::Packet;
use oss_proto::billing::{BillerToServer, ServerToBiller};
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, LoginResponse, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::config::MemSource;
use oss_server::modules::billing_ssc::{BillerConnector, BillerLink, LinkEvent, LinkState};
use oss_server::modules::core as core_mod;
use oss_server::modules::stats::{Stats, stat};
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/billing-ssc.txt");

fn vector(name: &str) -> Vec<u8> {
    let line = VECTORS
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some(name))
        .expect("vector");
    let mut f = line.split_whitespace();
    let len: usize = f.nth(2).and_then(|n| n.parse().ok()).expect("len");
    let hex = f.next().expect("hex");
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect();
    assert_eq!(bytes.len(), len);
    bytes
}

#[derive(Default)]
struct Script {
    to_zone: VecDeque<LinkEvent>,
    from_zone: Vec<Vec<u8>>,
    connects: usize,
    closed: bool,
}

struct FakeLink(Arc<Mutex<Script>>);

impl BillerLink for FakeLink {
    fn poll(&mut self, events: &mut Vec<LinkEvent>) {
        if let Ok(mut s) = self.0.lock() {
            events.extend(s.to_zone.drain(..));
        }
    }
    fn send(&mut self, packet: &[u8], _reliable: bool) {
        if let Ok(mut s) = self.0.lock() {
            s.from_zone.push(packet.to_vec());
        }
    }
    fn close(&mut self) {
        if let Ok(mut s) = self.0.lock() {
            s.closed = true;
        }
    }
}

struct FakeConnector(Arc<Mutex<Script>>);

impl BillerConnector for FakeConnector {
    fn connect(&self, _addr: SocketAddr) -> Option<Box<dyn BillerLink>> {
        if let Ok(mut s) = self.0.lock() {
            s.connects += 1;
            s.closed = false;
        }
        Some(Box::new(FakeLink(self.0.clone())))
    }
}

const GLOBAL: &str = "[Chat]\nMessageReliable = 1\n[Listen]\nPort = 5000\n\
[Billing]\nIP = 127.0.0.1\nPort = 1850\nServerID = 5000\nGroupID = 1\nScoreID = 5000\n\
ServerName = OSS Test Zone\nPassword = billpw\nRetryInterval = 5\n\
LocalChats = zonechat\nLocalChatPrefix = zz\n";

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn new(global: &str) -> Self {
        let src = MemSource::new([
            ("conf/global.conf", global.to_owned()),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_go = 1\ncmd_usage = 1\ncmd_userid = 1\ncmd_userdbadm = 1\ncmd_find = 1\n"
                    .to_owned(),
            ),
            (
                "arenas/(default)/arena.conf",
                "[Modules]\nAttachModules = \n".to_owned(),
            ),
        ]);
        let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
            .iter()
            .filter(|s| **s != "auth_file")
            .map(|s| (*s).to_owned())
            .collect();
        modules.push("billing_ssc".into());
        let zone = ServerBuilder::with_source(src)
            .modules(modules)
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

    fn take(&mut self, p: PlayerId) -> Vec<Vec<u8>> {
        self.lo.take(p).into_iter().map(|(d, _)| d).collect()
    }

    fn start_login(&mut self, name: &str, pw: &str, new_user: bool) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        let mut buf = Vec::new();
        Login {
            kind: c2s::kind::LOGIN,
            flags: u8::from(new_user),
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(pw.as_bytes()),
            machine_id: 0xDEAD_BEEF,
            connect_type: 4,
            timezone_bias: -60,
            unknown1: 0,
            client_version: 134,
            field444: 444,
            bound_ip: 0,
            permission_id: 99,
            server_ip: 0,
            bound_port: 0,
            redirect_id: 0,
            using_proxy: 0,
            unused: 0,
            cont_id: None,
        }
        .encode(&mut buf);
        self.send(p, &buf);
        p
    }

    fn login_code(&mut self, p: PlayerId) -> Option<u8> {
        self.take(p)
            .iter()
            .find(|d| d[0] == s2c::kind::LOGIN_RESPONSE)
            .map(|d| LoginResponse::decode(d).expect("login response").code)
    }

    fn enter(&mut self, p: PlayerId) {
        let mut buf = Vec::new();
        GoArena {
            ship: 8,
            allow_audio: 1,
            obscenity_filter: 0,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            arena: FixedStr::new(b""),
            optional_graphics: None,
        }
        .encode(&mut buf);
        self.send(p, &buf);
        self.tick(10);
        self.take(p);
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
}

fn chats(pkts: &[Vec<u8>]) -> Vec<(u8, String)> {
    pkts.iter()
        .filter(|d| d[0] == s2c::kind::CHAT)
        .map(|d| S2CChat::decode(d).expect("chat").0)
        .map(|c| (c.kind, String::from_utf8_lossy(&c.text).into_owned()))
        .collect()
}

fn scripted() -> (H, Arc<Mutex<Script>>) {
    let mut h = H::new(GLOBAL);
    let script = Arc::new(Mutex::new(Script::default()));
    h.zone.reg.provide::<dyn BillerConnector>(
        Owner(9999),
        Scope::Global,
        Arc::new(FakeConnector(script.clone())),
    );
    (h, script)
}

fn push(s: &Arc<Mutex<Script>>, ev: LinkEvent) {
    s.lock().expect("lock").to_zone.push_back(ev);
}

fn sent(s: &Arc<Mutex<Script>>) -> Vec<Vec<u8>> {
    std::mem::take(&mut s.lock().expect("lock").from_zone)
}

#[test]
fn scripted_biller() {
    let (mut h, s) = scripted();
    h.tick(10);
    assert_eq!(s.lock().expect("lock").connects, 1);
    push(&s, LinkEvent::Connected);
    h.tick(1);
    // S2B_ServerConnect, byte for byte as ASSS builds it
    assert_eq!(sent(&s), vec![vector("connect")]);

    // logins wait for the biller while it hasn't answered
    assert_eq!(
        oss_server::modules::billing_ssc::link_state(&h.zone),
        LinkState::WaitLogin
    );
    let early = h.start_login("early", "x", false);
    h.tick(10);
    assert!(h.login_code(early).is_some());
    assert!(
        h.zone
            .players
            .get(early)
            .is_some_and(|p| p.name == "^early")
    );

    // any packet from the biller completes its login
    push(&s, LinkEvent::Packet(vec![0x33, b'i', b'd']));
    h.tick(1);
    assert_eq!(sent(&s), vec![vector("capabilities")]);
    assert_eq!(
        oss_server::modules::billing_ssc::link_state(&h.zone),
        LinkState::LoggedIn
    );

    // player login goes to the biller
    let p = h.start_login("player", "pw", false);
    h.tick(1);
    assert_eq!(h.login_code(p), None);
    let out = sent(&s);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].len(), 86, "VIE login stops before ClientExtraData");
    let ServerToBiller::UserLogin(l) = ServerToBiller::decode(&out[0]).expect("decode") else {
        panic!("not a login");
    };
    assert_eq!(l.name.as_bytes(), b"player");
    assert_eq!(l.password.as_bytes(), b"pw");
    assert_eq!(l.connection_id, u32::from(p.0));
    assert_eq!(
        (l.machine_id, l.timezone, l.timezone_high),
        (0xDEAD_BEEF, -60, 0xFFFF)
    );
    assert_eq!(l.client_version, 134);

    // the biller's answer (ASSS vector, with our connection id)
    let mut ok = vector("login_ok");
    ok[2..6].copy_from_slice(&u32::from(p.0).to_le_bytes());
    push(&s, LinkEvent::Packet(ok));
    h.tick(10);
    assert_eq!(h.login_code(p), Some(0));
    {
        let pl = h.zone.players.get(p).expect("player");
        assert_eq!((pl.name.as_str(), pl.squad.as_str()), ("Player", "Squad"));
        assert!(pl.authenticated);
    }
    assert_eq!(
        oss_server::modules::billing_ssc::user_id(&h.zone, p),
        Some(4242)
    );
    h.enter(p);
    h.chat(p, chat_type::PUBLIC, 0, "?usage");
    let texts: Vec<String> = chats(&h.take(p)).into_iter().map(|c| c.1).collect();
    assert!(
        texts
            .iter()
            .any(|t| t == "first played: 08-04-2003 15:55:51")
    );
    assert!(texts.iter().any(|t| t.starts_with("  total:     1:00:")));

    // channel chat, remote privates and unknown commands go to the biller
    sent(&s);
    h.chat(p, chat_type::CHANNEL, 0, "hello");
    h.chat(p, chat_type::CHANNEL, 0, "2;yo");
    h.chat(p, chat_type::REMOTE_PRIVATE, 0, ":someone:hi there");
    h.chat(p, chat_type::PUBLIC, 0, "?squadlist");
    h.chat(p, chat_type::PUBLIC, 0, "?chat=zonechat,other");
    let got: Vec<ServerToBiller> = sent(&s)
        .iter()
        .map(|d| ServerToBiller::decode(d).expect("decode"))
        .collect();
    let cid = u32::from(p.0);
    let chan = |c: &[u8], t: &[u8]| ServerToBiller::UserChannelChat {
        connection_id: cid,
        channel: FixedStr::new(c),
        text: t.to_vec(),
    };
    let cmd = |t: &[u8]| ServerToBiller::UserCommand {
        connection_id: cid,
        text: t.to_vec(),
    };
    assert_eq!(
        got,
        vec![
            chan(b"1", b"hello"),
            chan(b"2", b"yo"),
            ServerToBiller::UserRemotePrivate {
                connection_id: u32::MAX,
                group_id: 1,
                kind: 2,
                sound: 0,
                text: b":someone:(Player)>hi there".to_vec(),
            },
            cmd(b"?squadlist"),
            cmd(b"?chat=$l$zz|zonechat,other"),
        ]
    );

    // messages from the biller
    h.take(p);
    let b2s = |pkt: BillerToServer| LinkEvent::Packet(pkt.to_bytes());
    push(
        &s,
        b2s(BillerToServer::ZoneMessage {
            source_server: 1,
            kind: 2,
            sound: 0,
            text: b":player:(bob)>hey".to_vec(),
        }),
    );
    push(
        &s,
        b2s(BillerToServer::ChannelMessage {
            connection_id: cid,
            channel: 3,
            text: b"bob> hi".to_vec(),
        }),
    );
    push(
        &s,
        b2s(BillerToServer::ArenaMessage {
            connection_id: cid,
            text: b"$l$zz|zonechat: Player".to_vec(),
        }),
    );
    push(
        &s,
        b2s(BillerToServer::UserPacket {
            connection_id: cid,
            data: vec![s2c::kind::LOGIN_TEXT, b'x', 0],
        }),
    );
    h.tick(1);
    let out = h.take(p);
    assert_eq!(
        chats(&out),
        vec![
            (chat_type::REMOTE_PRIVATE, "(bob)>hey".to_owned()),
            (chat_type::CHANNEL, "3:bob> hi".to_owned()),
            (chat_type::ARENA, "(local) zonechat: Player".to_owned()),
        ]
    );
    assert!(out.iter().any(|d| d == &[s2c::kind::LOGIN_TEXT, b'x', 0]));

    // a bad password, then the biller kicks the player out
    let q = h.start_login("other", "bad", false);
    h.tick(1);
    sent(&s);
    let mut bad = vector("login_invalidpw");
    bad[2..6].copy_from_slice(&u32::from(q.0).to_le_bytes());
    push(&s, LinkEvent::Packet(bad));
    h.tick(10);
    assert_eq!(
        h.login_code(q),
        Some(oss_server::modules::core::auth_code::BAD_PASSWORD)
    );

    let mut kick = vector("kickout");
    kick[1..5].copy_from_slice(&u32::from(p.0).to_le_bytes());
    push(&s, LinkEvent::Packet(kick));
    h.tick(50);
    let logoff: Vec<Vec<u8>> = sent(&s).into_iter().filter(|d| d[0] == 0x05).collect();
    assert_eq!(logoff.len(), 1);
    assert_eq!(logoff[0].len(), 15, "score unchanged: logoff without Score");
    assert_eq!(&logoff[0][1..5], &u32::from(p.0).to_le_bytes());

    // losing the biller: retry after Billing:RetryInterval
    push(&s, LinkEvent::Disconnected);
    h.tick(1);
    assert_eq!(
        oss_server::modules::billing_ssc::link_state(&h.zone),
        LinkState::Retry
    );
    h.tick(520);
    assert_eq!(s.lock().expect("lock").connects, 2);
}

#[test]
fn logoff_packets_match_asss() {
    // the zone's encoder against the ASSS layouts
    let full = ServerToBiller::UserLogoff {
        connection_id: 7,
        reason: 0,
        latency: 12,
        ping: 34,
        loss_s2c: 1,
        loss_c2s: 2,
        score: Some(oss_proto::billing::PlayerScore {
            kills: 10,
            deaths: 3,
            flags: 2,
            points: 1234,
            flag_points: 56,
        }),
    };
    assert_eq!(full.to_bytes(), vector("logoff_full"));
    let ServerToBiller::UserLogoff { score, .. } = &full else {
        unreachable!()
    };
    let mut short = full.clone();
    if let ServerToBiller::UserLogoff { score: s, .. } = &mut short {
        *s = None;
    }
    assert!(score.is_some());
    assert_eq!(short.to_bytes(), vector("logoff_noscore"));
    assert_eq!(
        ServerToBiller::decode(&vector("logoff_noscore")).expect("decode"),
        short
    );
    assert_eq!(ServerToBiller::Ping.to_bytes(), vector("ping"));
}

// ---- end to end ---------------------------------------------------------------

fn wait(zones: &mut [&mut H], mut done: impl FnMut(&mut [&mut H]) -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        for h in zones.iter_mut() {
            h.tick(1);
        }
        if done(zones) {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let states: Vec<_> = zones
        .iter()
        .map(|h| oss_server::modules::billing_ssc::link_state(&h.zone))
        .collect();
    panic!("timed out; link states {states:?}");
}

fn e2e_zone(biller: SocketAddr, server_id: u32) -> H {
    e2e_zone_pw(biller, server_id, "secret")
}

fn e2e_zone_pw(biller: SocketAddr, server_id: u32, pw: &str) -> H {
    let g = format!(
        "[Chat]\nMessageReliable = 1\n[Billing]\nIP = {}\nPort = {}\nServerID = {server_id}\n\
         ScoreID = 1\nServerName = zone{server_id}\nPassword = {pw}\n",
        biller.ip(),
        biller.port()
    );
    H::new(&g)
}

fn find_text(pkts: &[Vec<u8>], want: &str) -> bool {
    chats(pkts).iter().any(|(_, t)| t == want)
}

#[test]
fn two_zones_through_oss_biller() {
    use oss_biller::{Biller, BillerConfig, BillerHandle, Registration, Store};
    let biller = BillerHandle::spawn(
        ([127, 0, 0, 1], 0).into(),
        Biller::new(
            Store::in_memory().expect("db"),
            BillerConfig {
                password: Some("secret".into()),
                registration: Registration::Ask,
                ..BillerConfig::default()
            },
        ),
    )
    .expect("biller");
    let mut a = e2e_zone(biller.addr(), 1);
    let mut b = e2e_zone(biller.addr(), 2);
    let up = |hs: &mut [&mut H]| {
        hs.iter()
            .all(|h| oss_server::modules::billing_ssc::link_state(&h.zone) == LinkState::LoggedIn)
    };
    wait(&mut [&mut a, &mut b], up);

    // an unknown name is asked to register; then it is created
    let p = a.start_login("alice", "apw", false);
    let mut code = None;
    wait(&mut [&mut a], |hs| {
        code = hs[0].login_code(p);
        code.is_some()
    });
    assert_eq!(code, Some(oss_server::modules::core::auth_code::NEW_NAME));
    let alice = a.start_login("alice", "apw", true);
    let bob = b.start_login("bob", "bpw", true);
    let (mut ca, mut cb) = (None, None);
    wait(&mut [&mut a, &mut b], |hs| {
        ca = ca.or_else(|| hs[0].login_code(alice));
        cb = cb.or_else(|| hs[1].login_code(bob));
        ca.is_some() && cb.is_some()
    });
    assert_eq!((ca, cb), (Some(0), Some(0)));
    a.enter(alice);
    b.enter(bob);

    // chat channels across zones
    a.chat(alice, chat_type::PUBLIC, 0, "?chat=lobby");
    b.chat(bob, chat_type::PUBLIC, 0, "?chat=other,lobby");
    let mut got = Vec::new();
    wait(&mut [&mut a, &mut b], |hs| {
        got.extend(hs[1].take(bob));
        find_text(&got, "lobby: alice,bob")
    });
    a.chat(alice, chat_type::CHANNEL, 0, "hello all");
    got.clear();
    wait(&mut [&mut a, &mut b], |hs| {
        got.extend(hs[1].take(bob));
        find_text(&got, "2:alice> hello all")
    });

    // remote private message, and one to nobody
    a.take(alice);
    b.chat(bob, chat_type::REMOTE_PRIVATE, 0, ":alice:psst");
    b.chat(bob, chat_type::REMOTE_PRIVATE, 0, ":nobody:psst");
    let (mut ga, mut gb) = (Vec::new(), Vec::new());
    wait(&mut [&mut a, &mut b], |hs| {
        ga.extend(hs[0].take(alice));
        gb.extend(hs[1].take(bob));
        find_text(&ga, "(bob)>psst") && find_text(&gb, "nobody is not online.")
    });

    // squads
    a.chat(alice, chat_type::PUBLIC, 0, "?squadcreate=Hawks:sqpw");
    b.chat(bob, chat_type::PUBLIC, 0, "?squadjoin=hawks:wrong");
    let (mut ga, mut gb) = (Vec::new(), Vec::new());
    wait(&mut [&mut a, &mut b], |hs| {
        ga.extend(hs[0].take(alice));
        gb.extend(hs[1].take(bob));
        find_text(&ga, "Squad Hawks created. It shows from your next login.")
            && find_text(&gb, "Wrong squad password.")
    });
    b.chat(bob, chat_type::PUBLIC, 0, "?squadjoin=hawks:sqpw");
    b.chat(bob, chat_type::PUBLIC, 0, "?squadlist");
    b.chat(bob, chat_type::PUBLIC, 0, "?find alice");
    let mut gb = Vec::new();
    wait(&mut [&mut a, &mut b], |hs| {
        gb.extend(hs[1].take(bob));
        find_text(&gb, "Hawks: alice(online), bob(online)") && find_text(&gb, "alice is in zone1")
    });

    // logging in again elsewhere drops the first session; the squad shows now
    let alice2 = b.start_login("Alice", "apw", false);
    let mut c2 = None;
    wait(&mut [&mut a, &mut b], |hs| {
        c2 = c2.or_else(|| hs[1].login_code(alice2));
        c2.is_some() && hs[0].zone.players.get(alice).is_none()
    });
    assert_eq!(c2, Some(0));
    let pl = b.zone.players.get(alice2).expect("alice");
    assert_eq!((pl.name.as_str(), pl.squad.as_str()), ("alice", "Hawks"));

    // wrong password
    let bad = a.start_login("bob", "nope", false);
    let mut c3 = None;
    wait(&mut [&mut a, &mut b], |hs| {
        c3 = hs[0].login_code(bad);
        c3.is_some()
    });
    assert_eq!(c3, Some(oss_server::modules::core::auth_code::BAD_PASSWORD));

    // a zone with the wrong biller password is dropped
    let mut c = e2e_zone_pw(biller.addr(), 3, "wrong");
    wait(&mut [&mut c], |hs| {
        oss_server::modules::billing_ssc::link_state(&hs[0].zone) == LinkState::Retry
    });
    biller.stop();
}

/// What a logoff actually carries, and when.
///
/// `billing_ssc` sends the player's score at logoff only when it differs
/// from the one it last recorded -- and leaving a **public** arena records
/// it, so an ordinary public-arena logoff sends the short 15-byte packet and
/// the biller learns nothing. ASSS 1.4.4 `core/billing_ssc.c` does the same:
/// `update_score()` compares against `p->pkt.{killpoints,flagpoints,wins,
/// losses,flagscarried}` and `PA_LEAVEARENA` has just set `saved_score` from
/// the same stats. These two tests pin both sides of that, because the
/// difference is invisible from anywhere else.
fn logged_in_player(h: &mut H, s: &Arc<Mutex<Script>>, name: &str) -> PlayerId {
    h.tick(10);
    push(s, LinkEvent::Connected);
    h.tick(1);
    push(s, LinkEvent::Packet(vec![0x33, b'i', b'd']));
    h.tick(1);
    let p = h.start_login(name, "pw", false);
    h.tick(1);
    let mut ok = vector("login_ok");
    ok[2..6].copy_from_slice(&u32::from(p.0).to_le_bytes());
    push(s, LinkEvent::Packet(ok));
    h.tick(10);
    assert_eq!(h.login_code(p), Some(0));
    p
}

fn logoff_of(h: &mut H, s: &Arc<Mutex<Script>>, p: PlayerId) -> ServerToBiller {
    sent(s);
    core_mod::kick(&mut h.zone, p);
    h.tick(50);
    let mut found: Vec<ServerToBiller> = sent(s)
        .iter()
        .filter(|d| d[0] == 0x05)
        .map(|d| ServerToBiller::decode(d).expect("logoff"))
        .collect();
    assert_eq!(found.len(), 1, "exactly one logoff");
    found.remove(0)
}

#[test]
fn a_public_arena_score_does_not_reach_the_biller() {
    let (mut h, s) = scripted();
    let p = logged_in_player(&mut h, &s, "player");
    h.enter(p); // no arena name: a public arena
    let stats = h.zone.iface::<dyn Stats>().expect("stats");
    stats.set(&mut h.zone, p, stat::KILLS, 1);
    stats.set(&mut h.zone, p, stat::KILL_POINTS, 11);
    assert_eq!(
        h.zone
            .players
            .get(p)
            .map(|x| (x.score.wins, x.score.kill_points)),
        Some((1, 11)),
        "the zone knows the score"
    );
    let ServerToBiller::UserLogoff { score, .. } = logoff_of(&mut h, &s, p) else {
        panic!("not a logoff");
    };
    assert_eq!(
        score, None,
        "a kill in a public arena is not sent: LeaveArena recorded it first"
    );
}

#[test]
fn a_flag_pickup_is_what_lets_a_public_score_through() {
    // The two sides of the comparison read `flags` from different places:
    // the recorded one from `stat::FLAG_PICKUPS`, the current one from the
    // player's `flags_carried`. Any flag picked up and no longer held makes
    // them differ, and then the whole score goes after all.
    let (mut h, s) = scripted();
    let p = logged_in_player(&mut h, &s, "player");
    h.enter(p);
    let stats = h.zone.iface::<dyn Stats>().expect("stats");
    stats.set(&mut h.zone, p, stat::KILLS, 1);
    stats.set(&mut h.zone, p, stat::KILL_POINTS, 11);
    stats.set(&mut h.zone, p, stat::FLAG_PICKUPS, 1);
    let ServerToBiller::UserLogoff { score, .. } = logoff_of(&mut h, &s, p) else {
        panic!("not a logoff");
    };
    let score = score.expect("the score is sent once the flag counts disagree");
    assert_eq!((score.kills, score.points), (1, 11));
    assert_eq!(score.flags, 0, "flags carried, not flags picked up");
}
