//! `banners` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/banners.txt`, from `core/banners.c` compiled
//! unmodified).
//!
//! The rule a port is most likely to flatten is that the status is a
//! three-state value, not a boolean: a banner set before the player reaches
//! S_PLAYING is held at 1 and sent only when they enter an arena. With a
//! has-banner flag you either send it too early or lose a biller's banner.

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::s2c::{self, S2CChat};
use oss_proto::wire::FixedStr;

use std::sync::{Arc, Mutex};

use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::banners::{self, BANNER_LEN, BannerSet};
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId, PlayerStatus, Ship};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/banners.txt");

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    arena: ArenaId,
    other: Option<ArenaId>,
    players: Vec<PlayerId>,
    cbs: Arc<Mutex<Vec<String>>>,
    seq: usize,
    /// Packets each player received during the step just run.
    got: Vec<Vec<Vec<u8>>>,
}

impl H {
    fn new() -> Self {
        let src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\ncmd_go = 1\n".to_owned()),
            ("conf/staff.conf", String::new()),
            (
                "arenas/(default)/arena.conf",
                "[Team]\nDesiredTeams = 2\n".to_owned(),
            ),
        ]);
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        let cbs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let c = cbs.clone();
        zone.on::<BannerSet>(Owner(989), Scope::Global, move |_, _| {
            c.lock().expect("lock").push("setbanner".to_owned());
        });
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            arena: ArenaId(0),
            other: None,
            players: Vec::new(),
            cbs,
            seq: 0,
            got: Vec::new(),
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

    fn join(&mut self, name: &str, arena: &str) -> PlayerId {
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
        self.tick(5);
        buf.clear();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: if arena.is_empty() { -1 } else { -3 },
            arena: FixedStr::new(arena.as_bytes()),
            ..Default::default()
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.tick(10);
        p
    }

    /// `spwpscsn`: ship or spec, then where they are.
    fn seat(&mut self, spec: &str) {
        let chars: Vec<char> = spec.chars().collect();
        let pairs: Vec<Vec<char>> = chars
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(<[char]>::to_vec)
            .collect();
        // everyone joins first: joining ticks the clock, which would walk a
        // player seated as "not yet playing" on into Playing again
        for pair in &pairs {
            self.seq += 1;
            let name = format!("b{}", self.seq);
            let p = if pair[1] == 'o' {
                let id = self.join(&name, "other");
                self.other = self.zone.players.get(id).and_then(|x| x.arena);
                id
            } else {
                self.join(&name, "")
            };
            self.players.push(p);
        }
        for (i, pair) in pairs.iter().enumerate() {
            let p = self.players[i];
            if pair[1] == 'p' || pair[1] == 'o' {
                self.arena = match pair[1] {
                    'p' => self
                        .zone
                        .players
                        .get(p)
                        .and_then(|x| x.arena)
                        .unwrap_or(self.arena),
                    _ => self.arena,
                };
            }
            if let Some(x) = self.zone.players.get_mut(p) {
                x.ship = if pair[0] == 'w' { Ship(0) } else { Ship::SPEC };
                match pair[1] {
                    // connected but not yet playing
                    'c' => x.status = PlayerStatus::ArenaRespAndCbs,
                    // no arena at all
                    'n' => {
                        x.arena = None;
                        x.status = PlayerStatus::Connected;
                    }
                    _ => x.status = PlayerStatus::Playing,
                }
            }
        }
        self.capture();
        self.cbs.lock().expect("lock").clear();
    }

    fn capture(&mut self) {
        self.lo.collect(&mut self.zone);
        self.got = Vec::new();
        for i in 0..self.players.len() {
            let p = self.players[i];
            let packets: Vec<Vec<u8>> = self.lo.take(p).into_iter().map(|(d, _)| d).collect();
            self.got.push(packets);
        }
    }

    fn banners_seen(&self, i: usize) -> Vec<(i16, u8, u8, usize)> {
        self.got
            .get(i)
            .map(|v| {
                v.iter()
                    .filter(|d| d[0] == s2c::kind::BANNER)
                    .filter_map(|d| {
                        let b = s2c::Banner::decode(d).ok()?;
                        Some((b.pid, b.banner.0[0], b.banner.0[BANNER_LEN - 1], d.len()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn chats(&self, i: usize) -> Vec<String> {
        self.got
            .get(i)
            .map(|v| {
                v.iter()
                    .filter(|d| d[0] == s2c::kind::CHAT)
                    .filter_map(|d| S2CChat::decode(d).ok())
                    .map(|c| String::from_utf8_lossy(&c.0.text).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn field(rest: &str, key: &str) -> Option<String> {
    rest.split_whitespace()
        .find_map(|w| w.strip_prefix(key).map(str::to_owned))
}

#[test]
fn banners_match_asss() {
    let mut h = H::new();
    let mut scenarios = 0usize;
    let mut checked = 0usize;
    let mut cb_at = 0usize;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "seat" => {
                h = H::new();
                h.seat(rest.trim());
                cb_at = 0;
                scenarios += 1;
            }
            "c2s" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let i: usize = v[0].parse().expect("player");
                let fill = u8::from_str_radix(&field(rest, "fill=").expect("fill"), 16)
                    .expect("fill byte");
                let len: usize = field(rest, "len=")
                    .and_then(|n| n.parse().ok())
                    .expect("len");
                let mut pkt = vec![c2s::kind::BANNER];
                pkt.resize(len, fill);
                let p = h.players[i];
                h.zone.handle_packet(p, &pkt);
                h.capture();
            }
            "setbanner" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let i: usize = v[0].parse().expect("player");
                let fill = u8::from_str_radix(&field(rest, "fill=").expect("fill"), 16)
                    .expect("fill byte");
                let biller = field(rest, "frombiller=").as_deref() == Some("1");
                let p = h.players[i];
                banners::set_banner(&mut h.zone, p, &[fill; BANNER_LEN], biller);
                h.capture();
            }
            "enter" => {
                let i: usize = rest.trim().parse().expect("player");
                let p = h.players[i];
                let arena = h.zone.players.get(p).and_then(|x| x.arena);
                h.zone.fire(&oss_server::modules::core::PlayerAction {
                    player: p,
                    action: oss_server::modules::core::Action::EnterArena,
                    arena,
                });
                h.capture();
            }
            "state" => {
                for item in rest.split_whitespace() {
                    let Some((who, body)) = item.split_once(':') else {
                        continue;
                    };
                    let i: usize = who.parse().expect("player index");
                    let (status, first) = body.split_once('/').expect("status/banner");
                    let p = h.players[i];
                    let (got_status, got_banner) = banners::banner_state(&h.zone, p);
                    assert_eq!(got_status.to_string(), status, "p{i}: banner status");
                    assert_eq!(
                        format!("{:02x}", got_banner[0]),
                        first,
                        "p{i}: stored banner"
                    );
                    checked += 2;
                }
            }
            "pkt" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let (who, rest_at) = if v[0] == "one" {
                    (Some(v[1].parse::<usize>().expect("player")), 2)
                } else {
                    (None, 1)
                };
                assert_eq!(v[rest_at], "1f", "only banner packets are recorded");
                let pid: i16 = field(rest, "pid=")
                    .and_then(|n| n.parse().ok())
                    .expect("pid");
                let len: usize = field(rest, "len=")
                    .and_then(|n| n.parse().ok())
                    .expect("len");
                let bytes = field(rest, "banner=").expect("banner");
                let (first, last) = bytes.split_once("..").expect("first..last");
                let want = (
                    pid,
                    u8::from_str_radix(first, 16).expect("first"),
                    u8::from_str_radix(last, 16).expect("last"),
                    len,
                );
                // The transcript's pid field is a player index, and
                // `normalise` puts our packets into that same numbering, so
                // these compare directly.
                match who {
                    Some(i) => {
                        let got = normalise(&h, h.banners_seen(i));
                        assert!(got.contains(&want), "p{i} never got {want:?}, only {got:?}");
                    }
                    None => {
                        // A broadcast goes to the banner owner's arena,
                        // which is not always the main one: a scenario
                        // seats a player in a different arena on purpose.
                        let owner = usize::try_from(want.0).unwrap_or(0);
                        let arena = h
                            .zone
                            .players
                            .get(h.players[owner])
                            .and_then(|x| x.arena)
                            .unwrap_or(h.arena);
                        let in_arena: Vec<usize> = (0..h.players.len())
                            .filter(|&i| {
                                h.zone
                                    .players
                                    .get(h.players[i])
                                    .is_some_and(|x| x.is_playing_in(arena))
                            })
                            .collect();
                        assert!(!in_arena.is_empty(), "nobody is in the arena");
                        for i in in_arena {
                            let got = normalise(&h, h.banners_seen(i));
                            assert!(
                                got.contains(&want),
                                "p{i} never got the broadcast {want:?}, only {got:?}"
                            );
                        }
                    }
                }
                checked += 1;
            }
            "cb" => {
                let all = h.cbs.lock().expect("lock").clone();
                assert_eq!(
                    all.get(cb_at).map(String::as_str),
                    Some(rest.trim()),
                    "callback {cb_at}, all of them {all:?}"
                );
                cb_at += 1;
                checked += 1;
            }
            "chat" => {
                // the only chat here is the refusal, which goes to the
                // player whose packet was refused
                let mut found = false;
                for i in 0..h.players.len() {
                    if h.chats(i).iter().any(|t| t == rest.trim()) {
                        found = true;
                    }
                }
                assert!(found, "nobody heard {:?}", rest.trim());
                checked += 1;
            }
            _ => {}
        }
    }
    assert!(scenarios >= 5, "only {scenarios} scenarios replayed");
    assert!(checked > 80, "only {checked} records checked");
}

/// Puts the transcript's player numbering back on a banner packet's pid.
fn normalise(h: &H, seen: Vec<(i16, u8, u8, usize)>) -> Vec<(i16, u8, u8, usize)> {
    seen.into_iter()
        .map(|(pid, a, b, len)| {
            let n = h
                .players
                .iter()
                .position(|p| i16::try_from(p.0).unwrap_or(-1) == pid)
                .map(|i| i as i16)
                .unwrap_or(pid);
            (n, a, b, len)
        })
        .collect()
}
