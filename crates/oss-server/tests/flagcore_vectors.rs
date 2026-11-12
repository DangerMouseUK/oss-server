//! `flagcore` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/flagcore.txt`, from `core/flagcore.c` compiled
//! unmodified, with a recording flag game in place of the real ones).
//!
//! The cell of the 3×3 state table most likely to be wrong is taking one
//! flag off a carrier holding several: flagcore cannot drop a single flag,
//! so it sends a DROP for the carrier — which drops all of them on the
//! client — and then a PICKUP for every other flag that carrier still
//! holds. `flagscarried` is zeroed and counted back up, never decremented.

use std::sync::{Arc, Mutex};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::wire::FixedStr;

use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::core as core_mod;
use oss_server::modules::flagcore::{
    self, Cleanup, FlagGain, FlagGame, FlagInfo, FlagLost, FlagOnMap, FlagReset, FlagState,
};
use oss_server::modules::game::KillAnnounced;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId, Ship};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/flagcore.txt");

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

/// Logs what flagcore asks of the game and does nothing else, like the
/// recorder the transcript was made with.
struct Recorder {
    calls: Arc<Mutex<Vec<String>>>,
    mode: i32,
    reserve: usize,
    names: Arc<Mutex<Vec<PlayerId>>>,
}

impl Recorder {
    fn who(&self, p: PlayerId) -> String {
        self.names
            .lock()
            .expect("lock")
            .iter()
            .position(|x| *x == p)
            .map(|i| format!("p{i}"))
            .unwrap_or_else(|| "-".to_owned())
    }
}

impl FlagGame for Recorder {
    fn init(&self, zone: &mut Zone, arena: ArenaId) {
        self.calls.lock().expect("lock").push("Init".to_owned());
        flagcore::set_carry_mode(zone, arena, self.mode);
        flagcore::reserve_flags(zone, arena, self.reserve);
    }

    fn flag_touch(&self, _zone: &mut Zone, _arena: ArenaId, player: PlayerId, fid: usize) {
        self.calls
            .lock()
            .expect("lock")
            .push(format!("FlagTouch {} {fid}", self.who(player)));
    }

    fn cleanup(
        &self,
        _zone: &mut Zone,
        _arena: ArenaId,
        fid: usize,
        reason: Cleanup,
        carrier: PlayerId,
        freq: i16,
    ) {
        self.calls.lock().expect("lock").push(format!(
            "Cleanup {fid} reason={} carrier={} freq={freq}",
            reason as u8,
            self.who(carrier)
        ));
    }
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    arena: ArenaId,
    players: Vec<PlayerId>,
    names: Arc<Mutex<Vec<PlayerId>>>,
    calls: Arc<Mutex<Vec<String>>>,
    cbs: Arc<Mutex<Vec<String>>>,
    ends: Arc<Mutex<usize>>,
    /// Packets sent to the arena during the step just run.
    got: Vec<Vec<u8>>,
    /// Brought the arena into being; sits out every scenario.
    boot: PlayerId,
}

impl H {
    fn new(mode: i32, reserve: usize) -> Self {
        let src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\n".to_owned()),
            ("conf/staff.conf", String::new()),
            (
                "arenas/(default)/arena.conf",
                "[Team]\nDesiredTeams = 2\n".to_owned(),
            ),
        ]);
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(oss_server::prng::Prng::new(1));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let names = Arc::new(Mutex::new(Vec::new()));
        let cbs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ends = Arc::new(Mutex::new(0usize));
        // provided globally, before any arena exists: flagcore looks the
        // game up per arena and falls back to the global provider
        zone.reg.provide::<dyn FlagGame>(
            Owner(995),
            Scope::Global,
            Arc::new(Recorder {
                calls: calls.clone(),
                mode,
                reserve,
                names: names.clone(),
            }),
        );
        let c = cbs.clone();
        zone.on::<FlagGain>(Owner(994), Scope::Global, move |_, _| {
            c.lock().expect("lock").push("flaggain".to_owned());
        });
        let c = cbs.clone();
        zone.on::<FlagLost>(Owner(994), Scope::Global, move |_, _| {
            c.lock().expect("lock").push("flaglost".to_owned());
        });
        let c = cbs.clone();
        zone.on::<FlagOnMap>(Owner(994), Scope::Global, move |_, _| {
            c.lock().expect("lock").push("flagonmap".to_owned());
        });
        let c = cbs.clone();
        zone.on::<FlagReset>(Owner(994), Scope::Global, move |_, _| {
            c.lock().expect("lock").push("flagreset".to_owned());
        });
        let e = ends.clone();
        zone.on::<oss_server::modules::persist::IntervalEnded>(
            Owner(993),
            Scope::Global,
            move |_, ev| {
                if ev.interval == oss_server::modules::persist::Interval::Game {
                    *e.lock().expect("lock") += 1;
                }
            },
        );
        let mut h = Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            arena: ArenaId(0),
            players: Vec::new(),
            names,
            calls,
            cbs,
            ends,
            got: Vec::new(),
            boot: PlayerId(0),
        };
        // The transcript creates the arena and runs the game's Init before
        // it seats anyone, and an arena here is made on demand, so one
        // player brings it into being and then sits out as a spectator.
        let boot = h.join("boot");
        h.arena = h
            .zone
            .players
            .get(boot)
            .and_then(|x| x.arena)
            .expect("arena");
        if let Some(x) = h.zone.players.get_mut(boot) {
            x.ship = Ship::SPEC;
        }
        let _ = h.lo.take(boot);
        h.boot = boot;
        h
    }

    fn tick(&mut self, n: u64) {
        for _ in 0..n {
            self.t += 1;
            self.zone.set_now(Ticks(self.t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
    }

    fn join(&mut self, name: &str) -> PlayerId {
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

    /// `0w.0w.1w.` — freq digit, ship or spec, then where they are.
    fn seat(&mut self, spec: &str) {
        let chars: Vec<char> = spec.chars().collect();
        let places: Vec<Vec<char>> = chars
            .chunks(3)
            .filter(|c| c.len() == 3)
            .map(<[char]>::to_vec)
            .collect();
        for i in 0..places.len() {
            let p = self.join(&format!("p{i}"));
            self.players.push(p);
        }
        for (i, c) in places.into_iter().enumerate() {
            let p = self.players[i];
            if let Some(x) = self.zone.players.get_mut(p) {
                x.freq = c[0].to_digit(10).unwrap_or(0) as u16;
                x.ship = if c[1] == 'w' { Ship(0) } else { Ship::SPEC };
                x.pos.status = if c[2] == 'z' { 0x20 } else { 0 };
            }
        }
        *self.names.lock().expect("lock") = self.players.clone();
        self.capture();
    }

    /// Takes the packets sent to the arena since the last capture. Every
    /// player in these scenarios is in the one arena, so player 0's queue
    /// is the arena's.
    fn capture(&mut self) {
        self.lo.collect(&mut self.zone);
        let boot = self.boot;
        let out: Vec<Vec<u8>> = self.lo.take(boot).into_iter().map(|(d, _)| d).collect();
        for i in 0..self.players.len() {
            let p = self.players[i];
            let _ = self.lo.take(p);
        }
        // only flag packets: entering an arena sends plenty besides
        self.got = out
            .into_iter()
            .filter(|d| matches!(d[0], 0x12 | 0x13 | 0x14 | 0x16 | 0x22))
            .map(|d| self.renumber(d))
            .collect();
    }

    /// The transcript numbers players from 0 and puts those numbers in the
    /// packets. The bootstrap player holds our id 0, so every seated player
    /// is one higher; put the transcript's numbering back before comparing.
    /// Only two flag packets carry a player id: 0x13 pickup (bytes 3-4) and
    /// 0x16 drop (bytes 1-2).
    fn renumber(&self, mut d: Vec<u8>) -> Vec<u8> {
        let at = match d[0] {
            0x13 => 3,
            0x16 => 1,
            _ => return d,
        };
        if d.len() < at + 2 {
            return d;
        }
        let pid = i16::from_le_bytes([d[at], d[at + 1]]);
        if let Some(i) = self
            .players
            .iter()
            .position(|p| i16::try_from(p.0).unwrap_or(-1) == pid)
        {
            let n = i16::try_from(i).unwrap_or(0).to_le_bytes();
            d[at] = n[0];
            d[at + 1] = n[1];
        }
        d
    }

    fn flags_line(&self) -> String {
        let n = flagcore::flag_count(&self.zone, self.arena);
        let mut parts = Vec::new();
        for fid in 0..n {
            let f = flagcore::get_flag(&self.zone, self.arena, fid).unwrap_or_default();
            let state = match f.state {
                FlagState::None => "none",
                FlagState::OnMap => "onmap",
                FlagState::Carried => "carried",
            };
            let carrier = f
                .carrier
                .and_then(|c| self.players.iter().position(|x| *x == c))
                .map(|i| format!("p{i}"))
                .unwrap_or_else(|| "-".to_owned());
            parts.push(format!(
                "{fid}:{state}/{carrier}/{}/{}/{}",
                f.x, f.y, f.freq
            ));
        }
        format!("{n} {}", parts.join(" "))
    }
}

#[test]
fn flagcore_matches_asss() {
    let mut h = H::new(1, 0);
    let mut label = String::new();
    let mut last_comment = String::new();
    let mut scenarios = 0usize;
    let mut checked = 0usize;
    let mut cb_at = 0usize;
    let mut call_at = 0usize;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(c) = line.strip_prefix("# ") {
            last_comment = c.to_owned();
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "arena" => {
                label = std::mem::take(&mut last_comment);
                let num = |k: &str| -> i64 {
                    rest.split_whitespace()
                        .find_map(|w| w.strip_prefix(k)?.parse().ok())
                        .unwrap_or(0)
                };
                h = H::new(num("carrymode=") as i32, num("reserved=") as usize);
                cb_at = 0;
                call_at = 0;
                scenarios += 1;
            }
            "seat" => {
                h.seat(rest.trim());
                cb_at = h.cbs.lock().expect("lock").len();
                call_at = h.calls.lock().expect("lock").len();
            }
            "set" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let fid: usize = v[0].parse().expect("flag id");
                let state = match v[1] {
                    "onmap" => FlagState::OnMap,
                    "carried" => FlagState::Carried,
                    _ => FlagState::None,
                };
                let carrier = (v[2] != "-").then(|| {
                    let i: usize = v[2].trim_start_matches('p').parse().expect("carrier");
                    h.players[i]
                });
                let fi = FlagInfo {
                    state,
                    carrier,
                    x: v[3].parse().expect("x"),
                    y: v[4].parse().expect("y"),
                    freq: v[5].parse().expect("freq"),
                };
                let arena = h.arena;
                flagcore::set_flag(&mut h.zone, arena, fid, fi);
                h.capture();
            }
            "setret" => {
                // our SetFlags takes one record at a time, so it always
                // consumes exactly one -- including the records ASSS
                // refuses, which it still counts as consumed
                assert_eq!(rest.trim(), "1", "{label}: a multi-record SetFlags");
                checked += 1;
            }
            "touch" => {
                let v: Vec<usize> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let p = h.players[v[0]];
                let mut pkt = vec![c2s::kind::PICKUP_FLAG];
                pkt.extend_from_slice(&(v[1] as u16).to_le_bytes());
                h.zone.handle_packet(p, &pkt);
                h.capture();
            }
            "kill" => {
                let v: Vec<usize> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let (killer, killed) = (h.players[v[0]], h.players[v[1]]);
                let arena = h.arena;
                // flagcore moves flags on the announcement, after the
                // kill itself has been scored
                h.zone.fire(&KillAnnounced {
                    arena,
                    killer,
                    killed,
                    points: 0,
                    flags: 0,
                    green: 0,
                });
                h.capture();
            }
            "reset" => {
                let v: Vec<i32> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let arena = h.arena;
                *h.ends.lock().expect("lock") = 0;
                flagcore::flag_reset(&mut h.zone, arena, v[0] as i16, v[1]);
                h.capture();
            }
            "endintervals" => {
                let want: usize = rest.trim().parse().expect("count");
                assert_eq!(
                    *h.ends.lock().expect("lock"),
                    want,
                    "{label}: game intervals ended"
                );
                checked += 1;
            }
            "flags" => {
                assert_eq!(h.flags_line(), rest.trim(), "{label}: flag state");
                checked += 1;
            }
            "carried" => {
                for item in rest.split_whitespace() {
                    let Some((who, n)) = item.split_once(':') else {
                        continue;
                    };
                    let i: usize = who.parse().expect("player index");
                    let got = h
                        .zone
                        .players
                        .get(h.players[i])
                        .map_or(0, |x| x.flags_carried);
                    assert_eq!(got.to_string(), n, "{label}: p{i} flagscarried");
                    checked += 1;
                }
            }
            "counts" => {
                for item in rest.split_whitespace() {
                    let Some((who, n)) = item.split_once(':') else {
                        continue;
                    };
                    let want: usize = n.parse().expect("count");
                    let got = if let Some(f) = who.strip_prefix("freq") {
                        let freq: i16 = f.parse().expect("freq");
                        flagcore::count_freq_flags(&h.zone, h.arena, freq)
                    } else {
                        let i: usize = who.trim_start_matches('p').parse().expect("player");
                        flagcore::count_player_flags(&h.zone, h.arena, h.players[i])
                    };
                    assert_eq!(got, want, "{label}: {who}");
                    checked += 1;
                }
            }
            "pkt" => {
                let want = unhex(rest.trim());
                assert!(
                    h.got.contains(&want),
                    "{label}: {} never went out, only {:?}",
                    rest.trim(),
                    h.got
                );
                checked += 1;
            }
            "cb" => {
                let all = h.cbs.lock().expect("lock").clone();
                assert_eq!(
                    all.get(cb_at).map(String::as_str),
                    Some(rest.trim()),
                    "{label}: callback {cb_at}, all of them {all:?}"
                );
                cb_at += 1;
                checked += 1;
            }
            "game" => {
                let all = h.calls.lock().expect("lock").clone();
                // the transcript names players p0..pN and so does the
                // recorder, so these compare directly
                assert_eq!(
                    all.get(call_at).map(String::as_str),
                    Some(rest.trim()),
                    "{label}: game call {call_at}, all of them {all:?}"
                );
                call_at += 1;
                checked += 1;
            }
            // Not asserted. Our carry-arena lines are built by chatnet from
            // the victory packet rather than sent from flagcore, and this
            // harness has no chat client to receive either kind.
            "chatnet" => {}
            _ => {}
        }
    }
    assert!(scenarios >= 10, "only {scenarios} scenarios replayed");
    assert!(checked > 150, "only {checked} records checked");
}
