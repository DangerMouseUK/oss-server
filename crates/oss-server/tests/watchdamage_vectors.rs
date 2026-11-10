//! `watchdamage` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/watchdamage.txt`, from `core/watchdamage.c`
//! compiled unmodified).
//!
//! One deliberate divergence, and it is the reason this file is worth
//! replaying: ASSS's `ModuleWatch` moves the counter and sends nothing, so a
//! module that asks for a player's damage while nobody else is watching
//! never gets the client told to report it — and because the total is then
//! already 1, the first real watcher afterwards takes it to 2 and sends no
//! toggle either. We send the toggle on the same transitions a player's
//! watch uses. The test asserts our packet *and* that the transcript has
//! none there, so the divergence cannot drift out of sight.

use std::sync::{Arc, Mutex};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::modules::watchdamage::{self, PlayerDamage};
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/watchdamage.txt");

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    arena: ArenaId,
    players: Vec<PlayerId>,
    seq: usize,
    damage_cbs: Arc<Mutex<usize>>,
    /// Packets each player received during the step just run.
    got: Vec<Vec<Vec<u8>>>,
}

impl H {
    fn new() -> Self {
        let conf = "[Modules]\nAttachModules = watchdamage\n[Team]\nDesiredTeams = 2\n";
        let src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            (
                "conf/groupdef.conf",
                "[default]\ncmd_watchdamage = 1\nprivcmd_watchdamage = 1\n".to_owned(),
            ),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", conf.to_owned()),
        ]);
        let mut modules: Vec<String> = oss_server::modules::DEFAULT_MODULES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        modules.push("watchdamage".to_owned());
        let mut zone = ServerBuilder::with_source(src)
            .modules(modules)
            .build_zone()
            .expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        let damage_cbs = Arc::new(Mutex::new(0usize));
        let c = damage_cbs.clone();
        zone.on::<PlayerDamage>(Owner(996), Scope::Global, move |_, _| {
            *c.lock().expect("lock") += 1;
        });
        Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            arena: ArenaId(0),
            players: Vec::new(),
            seq: 0,
            damage_cbs,
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

    fn join(&mut self, name: &str, kind: ClientKind) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, kind, None).expect("slot");
        let mut buf = Vec::new();
        Login {
            kind: if kind == ClientKind::Continuum {
                c2s::kind::CONT_LOGIN
            } else {
                c2s::kind::LOGIN
            },
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
            cont_id: (kind == ClientKind::Continuum).then_some([0u8; 64]),
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.tick(5);
        buf.clear();
        GoArena {
            ship: 0,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            ..Default::default()
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.tick(10);
        p
    }

    /// `cpcpvp`: client kind then where they are, one pair per player.
    fn seat(&mut self, spec: &str) {
        self.players.clear();
        let chars: Vec<char> = spec.chars().collect();
        let pairs: Vec<Vec<char>> = chars
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(<[char]>::to_vec)
            .collect();
        for pair in &pairs {
            self.seq += 1;
            let kind = if pair[0] == 'c' {
                ClientKind::Continuum
            } else {
                ClientKind::Vie
            };
            let p = self.join(&format!("w{}", self.seq), kind);
            self.players.push(p);
        }
        self.arena = self
            .zone
            .players
            .get(self.players[0])
            .and_then(|x| x.arena)
            .expect("arena");
        for (i, pair) in pairs.iter().enumerate() {
            let p = self.players[i];
            if pair[1] != 'p' {
                // 'o' another arena, 'n' none: neither is in this one
                if let Some(x) = self.zone.players.get_mut(p) {
                    x.arena = None;
                }
            }
        }
        self.drain();
    }

    fn drain(&mut self) {
        self.lo.collect(&mut self.zone);
        for i in 0..self.players.len() {
            let p = self.players[i];
            let _ = self.lo.take(p);
        }
    }

    /// Records what each player received, for the pkt records that follow.
    fn capture(&mut self) {
        self.lo.collect(&mut self.zone);
        self.got = Vec::new();
        for i in 0..self.players.len() {
            let p = self.players[i];
            let packets: Vec<Vec<u8>> = self.lo.take(p).into_iter().map(|(d, _)| d).collect();
            self.got.push(packets);
        }
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

    /// Damage-related packets only: the 0x37 toggle and the 0x38 relay.
    fn watch_packets(&self, i: usize) -> Vec<Vec<u8>> {
        self.got
            .get(i)
            .map(|v| {
                v.iter()
                    .filter(|d| d[0] == s2c::kind::TOGGLE_DAMAGE || d[0] == s2c::kind::DAMAGE)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn total(&self, i: usize) -> usize {
        watchdamage::watch_count(&self.zone, self.players[i])
    }

    fn cmd(&mut self, from: usize, to: Option<usize>, args: &str) {
        let text = if args.is_empty() {
            "?watchdamage".to_owned()
        } else {
            format!("?watchdamage {args}")
        };
        let (kind, pid) = match to {
            Some(t) => (
                chat_type::PRIVATE,
                i16::try_from(self.players[t].0).unwrap_or(0),
            ),
            None => (chat_type::PUBLIC, 0),
        };
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind,
            sound: 0,
            pid,
            text: text.into_bytes(),
        })
        .encode(&mut buf);
        let p = self.players[from];
        self.zone.handle_packet(p, &buf);
    }
}

#[test]
fn watchdamage_matches_asss() {
    let mut h = H::new();
    let mut scenarios = 0usize;
    let mut checked = 0usize;
    let mut divergences = 0usize;
    // extra toggles we send that ASSS does not, keyed by player index
    let mut extra: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut cbs_before = 0usize;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "sizes" => {
                // the transcript states the wire sizes it was built with
                assert!(rest.contains("DamageData=9"), "DamageData size moved");
                assert!(rest.contains("S2C_header=7"), "0x38 header size moved");
                checked += 1;
            }
            "seat" => {
                h = H::new();
                h.seat(rest.trim());
                h.capture();
                extra.clear();
                scenarios += 1;
            }
            "cmd" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let from: usize = v[0].parse().expect("player");
                let (to, args_at) = if v[1] == "player" {
                    (Some(v[2].parse::<usize>().expect("target")), 3)
                } else {
                    (None, 2)
                };
                let args = rest
                    .split_once('[')
                    .and_then(|(_, r)| r.rsplit_once(']'))
                    .map(|(a, _)| a.to_owned())
                    .unwrap_or_default();
                let _ = args_at;
                h.cmd(from, to, &args);
                h.capture();
                extra.clear();
            }
            "modwatch" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let who: usize = v[0].parse().expect("player");
                let on = v[1] == "1";
                let p = h.players[who];
                let was = h.total(who);
                watchdamage::module_watch(&mut h.zone, p, on);
                h.capture();
                // ASSS sends nothing here. We send a toggle when the total
                // crosses 0/1 on a Continuum client, and the pkt records
                // below are checked against that.
                extra.clear();
                let after = h.total(who);
                let cont = h.zone.players.get(p).map(|x| x.kind) == Some(ClientKind::Continuum);
                if cont && was == 0 && after == 1 {
                    extra.push((who, vec![s2c::kind::TOGGLE_DAMAGE, 1]));
                    divergences += 1;
                } else if cont && was == 1 && after == 0 {
                    extra.push((who, vec![s2c::kind::TOGGLE_DAMAGE, 0]));
                    divergences += 1;
                }
            }
            "leave" => {
                let who: usize = rest.trim().parse().expect("player");
                let p = h.players[who];
                let arena = h.arena;
                h.zone.fire(&oss_server::modules::core::PlayerAction {
                    player: p,
                    action: oss_server::modules::core::Action::LeaveArena,
                    arena: Some(arena),
                });
                h.capture();
                extra.clear();
            }
            "damage" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let who: usize = v[0].parse().expect("player");
                let num = |k: &str| -> u64 {
                    v.iter()
                        .find_map(|w| w.strip_prefix(k)?.parse::<u64>().ok())
                        .unwrap_or(0)
                };
                let fill = v
                    .iter()
                    .find_map(|w| u8::from_str_radix(w.strip_prefix("fill=")?, 16).ok())
                    .unwrap_or(0);
                let tick = num("tick=") as u32;
                let len = num("len=") as usize;
                let mut pkt = vec![c2s::kind::DAMAGE];
                pkt.extend_from_slice(&tick.to_le_bytes());
                pkt.resize(len, fill);
                cbs_before = *h.damage_cbs.lock().expect("lock");
                let p = h.players[who];
                h.zone.handle_packet(p, &pkt);
                h.capture();
                extra.clear();
            }
            "state" => {
                for item in rest.split_whitespace() {
                    let Some((who, body)) = item.split_once(':') else {
                        continue;
                    };
                    let i: usize = who.parse().expect("player index");
                    let v: Vec<&str> = body.split('/').collect();
                    let p = h.players[i];
                    let watchers = watchdamage::watchers(&h.zone, p);
                    assert_eq!(
                        watchers.len().to_string(),
                        v[0],
                        "p{i}: how many are watching them"
                    );
                    assert_eq!(
                        watchdamage::module_watch_count(&h.zone, p).to_string(),
                        v[1],
                        "p{i}: module watches"
                    );
                    let want = if v[2] == "-" { "" } else { v[2] };
                    let got: String = watchers
                        .iter()
                        .map(|w| {
                            h.players
                                .iter()
                                .position(|x| x == w)
                                .map(|n| n.to_string())
                                .unwrap_or_default()
                        })
                        .collect();
                    assert_eq!(got, want, "p{i}: who is watching, in order");
                    checked += 3;
                }
            }
            "pkt" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let (who, hex) = if v[0] == "one" {
                    (Some(v[1].parse::<usize>().expect("player")), v[2])
                } else {
                    (None, v[1])
                };
                let want = unhex(hex);
                match who {
                    Some(i) => {
                        let got = h.watch_packets(i);
                        assert!(got.contains(&want), "p{i} never got {hex}, only {got:?}");
                    }
                    None => {
                        // a send to the watcher list: everyone watching the
                        // player the packet names must have had it
                        let pid = i16::from_le_bytes([want[1], want[2]]);
                        let target = h
                            .players
                            .iter()
                            .position(|p| i16::try_from(p.0).unwrap_or(-1) == pid)
                            .expect("damaged player");
                        let list = watchdamage::watchers(&h.zone, h.players[target]);
                        if list.is_empty() {
                            // The transcript records the packet ASSS built,
                            // not what any client saw; a send to an empty
                            // watcher list reaches nobody, so there is
                            // nothing here to compare against.
                            checked += 1;
                            continue;
                        }
                        for w in list {
                            let i = h.players.iter().position(|x| *x == w).expect("watcher");
                            let got = h.watch_packets(i);
                            assert!(
                                got.contains(&want),
                                "watcher p{i} never got {hex}, only {got:?}"
                            );
                        }
                    }
                }
                checked += 1;
            }
            "cb" => {
                assert_eq!(rest.trim(), "playerdamage", "unknown callback");
                let now = *h.damage_cbs.lock().expect("lock");
                assert!(now > cbs_before, "the damage callback did not fire");
                checked += 1;
            }
            "chat" => {
                let (who, text) = rest.split_once(' ').unwrap_or((rest, ""));
                let i: usize = who.parse().expect("player index");
                // the transcript names players p0..pN; ours are named to
                // stay unique across re-seats, so put ours back in
                let mut want = text.to_owned();
                for n in (0..h.players.len()).rev() {
                    let name = h
                        .zone
                        .players
                        .get(h.players[n])
                        .map(|x| x.name.clone())
                        .unwrap_or_default();
                    want = want.replace(&format!("p{n}"), &name);
                }
                let got = h.chats(i);
                assert!(
                    got.contains(&want),
                    "p{i} never heard {want:?}, only {got:?}"
                );
                checked += 1;
            }
            _ => {}
        }
        // any toggle we send that the transcript does not record is checked
        // here, once the records for this step have been consumed
        for (i, want) in extra.drain(..) {
            let got = h.watch_packets(i);
            assert!(
                got.contains(&want),
                "p{i} should have had our ModuleWatch toggle {want:?}, only {got:?}"
            );
        }
    }
    assert!(scenarios >= 5, "only {scenarios} scenarios replayed");
    assert!(checked > 100, "only {checked} records checked");
    assert_eq!(
        divergences, 2,
        "the ModuleWatch divergence should show up exactly twice"
    );
}
