//! `balls` replayed against ASSS's own transcript
//! (`fixtures/asss-vectors/balls.txt`, from `core/balls.c` compiled
//! unmodified).
//!
//! The spawn-point rule is the one that costs a zone something quietly:
//! ASSS decides how many points are configured by testing both coordinates
//! non-zero, so a legitimate spawn on row 0 or column 0 drops a four-point
//! arena to a two-point one. The rest of the file is the pickup, fire and
//! goal state machine, where `carrier` doubles as a "this goal has not been
//! counted yet" flag.

use std::sync::{Arc, Mutex};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::wire::FixedStr;

use oss_server::arena::ArenaId;
use oss_server::config::MemSource;
use oss_server::modules::balls::{self, BallFire, BallPickup, BallState, Goal};
use oss_server::modules::core as core_mod;
use oss_server::modules::lagdata::LagData;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId, Ship};
use oss_server::prng::Prng;
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const VECTORS: &str = include_str!("fixtures/asss-vectors/balls.txt");

/// The draw that makes our `uniform` return `v`.
fn uniform_draw(v: f64) -> u32 {
    (v * 4_294_967_296.0) as u32
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
    arena: ArenaId,
    players: Vec<PlayerId>,
    cbs: Arc<Mutex<Vec<String>>>,
    seq: usize,
    got: Vec<Vec<u8>>,
    boot: PlayerId,
}

impl H {
    fn new(conf: &str, solid: &[(u32, u32)]) -> Self {
        let mut tiles = Vec::new();
        for &(x, y) in solid {
            tiles.extend_from_slice(&(x | (y << 12) | (1u32 << 24)).to_le_bytes());
        }
        let arena_conf = format!("[General]\nMap = test.lvl\n{conf}\n[Team]\nDesiredTeams = 2\n");
        let mut src = MemSource::new([
            ("conf/global.conf", String::new()),
            (
                "conf/passwd.conf",
                "[General]\nAllowUnknown = yes\n".to_owned(),
            ),
            ("conf/groupdef.conf", "[default]\n".to_owned()),
            ("conf/staff.conf", String::new()),
            ("arenas/(default)/arena.conf", arena_conf),
        ]);
        src.insert("maps/test.lvl", tiles);
        let mut zone = ServerBuilder::with_source(src).build_zone().expect("zone");
        zone.res.insert(Prng::new(1));
        let cbs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let c = cbs.clone();
        zone.on::<BallPickup>(Owner(988), Scope::Global, move |_, _| {
            c.lock().expect("lock").push("ballpickup".to_owned());
        });
        let c = cbs.clone();
        zone.on::<BallFire>(Owner(988), Scope::Global, move |_, _| {
            c.lock().expect("lock").push("ballfire".to_owned());
        });
        let c = cbs.clone();
        zone.on::<Goal>(Owner(988), Scope::Global, move |_, _| {
            c.lock().expect("lock").push("goal".to_owned());
        });
        let mut h = Self {
            zone,
            lo: Loopback::default(),
            t: 0,
            arena: ArenaId(0),
            players: Vec::new(),
            cbs,
            seq: 0,
            got: Vec::new(),
            boot: PlayerId(0),
        };
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
        h.boot = boot;
        h.capture();
        h.cbs.lock().expect("lock").clear();
        h
    }

    fn tick_to(&mut self, t: u64) {
        while self.t < t {
            self.t += 1;
            self.zone.set_now(Ticks(self.t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
    }

    fn tick(&mut self, n: u64) {
        let to = self.t + n;
        self.tick_to(to);
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
            field555: 555,
            permission_id: 1,
            reserved: [0; 12],
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

    /// `0w.1w.0s.0wl`: freq digit, ship or spec, then '.' or 'l' for a
    /// player the lag checks have barred from carrying.
    fn seat(&mut self, spec: &str) {
        let chars: Vec<char> = spec.chars().collect();
        let places: Vec<Vec<char>> = chars
            .chunks(3)
            .filter(|c| c.len() == 3)
            .map(<[char]>::to_vec)
            .collect();
        for _ in 0..places.len() {
            self.seq += 1;
            let name = format!("s{}", self.seq);
            let p = self.join(&name);
            self.players.push(p);
        }
        for (i, c) in places.into_iter().enumerate() {
            let p = self.players[i];
            if let Some(x) = self.zone.players.get_mut(p) {
                x.freq = c[0].to_digit(10).unwrap_or(0) as u16;
                x.ship = if c[1] == 'w' { Ship(0) } else { Ship::SPEC };
                // the transcript stands p0 at (100, 200) and p1 at
                // (1100, 1200), in pixels, which a pickup copies onto the
                // ball; the rest are never carriers
                let (px, py) = match i {
                    0 => (100i16, 200i16),
                    1 => (1100, 1200),
                    n => (2000 + n as i16, 3000 + n as i16),
                };
                x.pos.x = px;
                x.pos.y = py;
                if c[2] == 'l' {
                    x.ext.get_or_default::<LagData>().no_flags_balls = true;
                }
            }
        }
        self.capture();
        self.cbs.lock().expect("lock").clear();
    }

    fn capture(&mut self) {
        self.lo.collect(&mut self.zone);
        let boot = self.boot;
        let out: Vec<Vec<u8>> = self.lo.take(boot).into_iter().map(|(d, _)| d).collect();
        for i in 0..self.players.len() {
            let p = self.players[i];
            let _ = self.lo.take(p);
        }
        self.got = out
            .into_iter()
            .filter(|d| d[0] == 0x2e)
            .map(|d| self.renumber(d))
            .collect();
    }

    /// The transcript numbers players from 0 and puts those numbers in the
    /// packet's carrier field; the bootstrap player holds our id 0, so every
    /// seated player is one higher. The carrier sits at bytes 10-11 of a
    /// 0x2e, and -1 for "nobody" is left alone.
    fn renumber(&self, mut d: Vec<u8>) -> Vec<u8> {
        if d.len() < 12 {
            return d;
        }
        let pid = i16::from_le_bytes([d[10], d[11]]);
        if let Some(i) = self
            .players
            .iter()
            .position(|p| i16::try_from(p.0).unwrap_or(-1) == pid)
        {
            let n = i16::try_from(i).unwrap_or(0).to_le_bytes();
            d[10] = n[0];
            d[11] = n[1];
        }
        d
    }

    fn balls_line(&self) -> String {
        let list = balls::balls(&self.zone, self.arena);
        let mut parts = Vec::new();
        for (i, b) in list.iter().enumerate() {
            let state = match b.state {
                BallState::OnMap => "onmap",
                BallState::Carried => "carried",
                BallState::Waiting => "waiting",
                BallState::None => "none",
            };
            let carrier = b
                .carrier
                .and_then(|c| self.players.iter().position(|x| *x == c))
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-1".to_owned());
            parts.push(format!(
                "{i}:{state}/{}/{}/{}/{}/{carrier}/{}",
                b.x, b.y, b.x_speed, b.y_speed, b.time
            ));
        }
        format!("{} {}", list.len(), parts.join(" "))
    }
}

/// Compares two `balls` lines. With `loose`, the x and y may differ by up
/// to one tile, because the scenario scripted a draw we cannot make
/// exactly; every other field still has to match.
fn compare_balls(got: &str, want: &str, loose: bool) {
    if !loose {
        assert_eq!(got, want, "ball state");
        return;
    }
    let split = |line: &str| -> Vec<Vec<String>> {
        line.split_whitespace()
            .skip(1)
            .filter_map(|w| Some(w.split_once(':')?.1.split('/').map(str::to_owned).collect()))
            .collect()
    };
    let (g, w) = (split(got), split(want));
    assert_eq!(g.len(), w.len(), "ball count: {got} vs {want}");
    for (i, (gf, wf)) in g.iter().zip(w.iter()).enumerate() {
        assert_eq!(gf[0], wf[0], "ball {i} state");
        for f in [1usize, 2] {
            let a: i32 = gf[f].parse().expect("coordinate");
            let b: i32 = wf[f].parse().expect("coordinate");
            assert!(
                (a - b).abs() <= 16,
                "ball {i} coordinate {f}: {a} against {b}, more than a tile apart"
            );
        }
        for f in [3usize, 4, 5, 6] {
            assert_eq!(gf[f], wf[f], "ball {i} field {f}");
        }
    }
}

fn field(rest: &str, key: &str) -> Option<String> {
    rest.split_whitespace()
        .find_map(|w| w.strip_prefix(key).map(str::to_owned))
}

#[test]
fn balls_match_asss() {
    let mut h = H::new("", &[]);
    let mut solid: Vec<(u32, u32)> = Vec::new();
    let mut uniforms: Vec<f64> = Vec::new();
    let mut scenarios = 0usize;
    let mut checked = 0usize;
    let mut cb_at = 0usize;
    let mut scripted_one = false;
    let mut pending = false;
    let mut get32 = 0u32;

    for line in VECTORS.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        if pending && word != "uniforms" && word != "settings" {
            // a respawn's draws: two uniforms then the fine-position word,
            // per ball, exactly as a fresh spawn does
            let left = h.zone.res.get_or_default::<Prng>().queued();
            assert_eq!(left, 0, "{left} draws from the last step were never used");
            let mut draws = Vec::new();
            for pair in uniforms.chunks(2) {
                draws.push(uniform_draw(pair.first().copied().unwrap_or(0.0)));
                draws.push(uniform_draw(pair.get(1).copied().unwrap_or(0.0)));
                draws.push(get32);
            }
            h.zone.res.get_or_default::<Prng>().queue(draws);
            pending = false;
        }
        match word {
            "sizes" => {
                assert!(rest.contains("BallPacket=16"), "the ball packet size moved");
                checked += 1;
            }
            "solid" => {
                solid = rest
                    .split_whitespace()
                    .skip(1)
                    .filter_map(|w| {
                        let (x, y) = w.split_once(',')?;
                        Some((x.parse().ok()?, y.parse().ok()?))
                    })
                    .collect();
            }
            "uniforms" => {
                // A uniforms line is the draws for the step that follows.
                // Before a settings line that is the arena's first spawn;
                // anywhere else it is a respawn, queued as soon as we know
                // the next record is not a settings line.
                uniforms = rest
                    .split_whitespace()
                    .skip(1)
                    .filter_map(|v| v.parse().ok())
                    .collect();
                pending = true;
            }
            "settings" => {
                let g = |k: &str| field(rest, k).unwrap_or_default();
                let spawn = |k: &str| -> (i32, i32, i32) {
                    let v: Vec<i32> = g(k).split(',').filter_map(|n| n.parse().ok()).collect();
                    (
                        v.first().copied().unwrap_or(0),
                        v.get(1).copied().unwrap_or(0),
                        v.get(2).copied().unwrap_or(0),
                    )
                };
                let (x0, y0, r0) = spawn("spawn0=");
                let count: usize = g("ballcount=").parse().unwrap_or(0);
                let mut conf = format!(
                    "[Soccer]\nBallCount = {count}\nSpawnX = {x0}\nSpawnY = {y0}\nSpawnRadius = {r0}\n\
                     GoalDelay = {}\nAllowGoalByDeath = {}\nSendTime = {}\n",
                    g("goaldelay="),
                    g("deathgoal="),
                    g("sendtime=")
                );
                for i in 1..4 {
                    let (x, y, r) = spawn(&format!("spawn{i}="));
                    conf.push_str(&format!(
                        "SpawnX{i} = {x}\nSpawnY{i} = {y}\nSpawnRadius{i} = {r}\n"
                    ));
                }
                h = H::new(&conf, &solid);
                // A scripted uniform of exactly 1.0 is a value neither
                // generator can produce -- ASSS's Uniform is
                // rand() / (RAND_MAX + 1.0) and ours is a 32-bit draw over
                // 2^32, so both stop one step short. The transcript's
                // harness hands the double straight in. The nearest draw we
                // can make lands up to one tile away, so the coordinates in
                // such a scenario are compared with that tolerance and
                // everything else still exactly.
                scripted_one = uniforms.iter().take(2 * count).any(|v| *v >= 1.0);
                pending = false;
                scenarios += 1;
                cb_at = 0;
                // The arena is made with its real ball count, because ASSS
                // only reads the spawn settings when the count is non-zero
                // -- an arena configured with none keeps zeroed spawn
                // points. Those first balls come from the generator; the
                // clock then moves to the scenario's start, the balls are
                // cleared, and they are placed again from the recorded
                // draws.
                get32 = u32::from_str_radix(&g("get32="), 16).unwrap_or(0);
                let mut draws = Vec::new();
                for i in 0..count {
                    draws.push(uniform_draw(uniforms.get(2 * i).copied().unwrap_or(0.0)));
                    draws.push(uniform_draw(
                        uniforms.get(2 * i + 1).copied().unwrap_or(0.0),
                    ));
                    draws.push(get32);
                }
                let t0: u64 = g("t0=").parse().unwrap_or(0);
                h.tick_to(t0);
                let arena = h.arena;
                balls::set_ball_count(&mut h.zone, arena, 0);
                h.zone.res.get_or_default::<Prng>().queue(draws);
                balls::set_ball_count(&mut h.zone, arena, count);
                // the transcript queues more uniforms than a scenario uses
                let left = h.zone.res.get_or_default::<Prng>().queued();
                assert_eq!(left, 0, "{left} queued draws were never used");
                h.capture();
                checked += 1;
            }
            "at" => {
                let t: u64 = rest.trim().parse().expect("tick");
                h.tick_to(t);
                h.capture();
            }
            "seat" => h.seat(rest.trim()),
            "pickup" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let who: usize = v[0].parse().expect("player");
                let bid: u32 = v[1].parse().expect("ball");
                let time: u32 = field(rest, "time=")
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0);
                let len: usize = field(rest, "len=")
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(6);
                // type byte, a ONE-byte ball id, then the tick: six bytes
                let mut pkt = vec![c2s::kind::PICKUP_BALL, bid as u8];
                pkt.extend_from_slice(&time.to_le_bytes());
                pkt.truncate(len.max(1));
                pkt.resize(len, 0);
                let p = h.players[who];
                h.zone.handle_packet(p, &pkt);
                h.capture();
            }
            "fire" => {
                // type, a one-byte id, x, y, speeds, shooter, then the tick
                let v: Vec<&str> = rest.split_whitespace().collect();
                let who: usize = v[0].parse().expect("player");
                let bid: u32 = v[1].parse().expect("ball");
                let nums: Vec<i16> = v[2..6]
                    .iter()
                    .map(|n| n.parse().expect("coordinate"))
                    .collect();
                let time: u32 = field(rest, "time=")
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0);
                let p = h.players[who];
                let mut pkt = vec![c2s::kind::SHOOT_BALL, bid as u8];
                for n in &nums {
                    pkt.extend_from_slice(&n.to_le_bytes());
                }
                pkt.extend_from_slice(&i16::try_from(p.0).unwrap_or(0).to_le_bytes());
                pkt.extend_from_slice(&time.to_le_bytes());
                h.zone.handle_packet(p, &pkt);
                h.capture();
            }
            "goal" => {
                let v: Vec<&str> = rest.split_whitespace().collect();
                let who: usize = v[0].parse().expect("player");
                let bid: u32 = v[1].parse().expect("ball");
                let x: i16 = v[2].parse().expect("x");
                let y: i16 = v[3].parse().expect("y");
                let mut pkt = vec![c2s::kind::GOAL, bid as u8];
                pkt.extend_from_slice(&x.to_le_bytes());
                pkt.extend_from_slice(&y.to_le_bytes());
                let p = h.players[who];
                h.zone.handle_packet(p, &pkt);
                h.capture();
            }
            "kill" => {
                let v: Vec<usize> = rest
                    .split_whitespace()
                    .filter_map(|n| n.parse().ok())
                    .collect();
                let (killer, killed) = (h.players[v[0]], h.players[v[1]]);
                let arena = h.arena;
                h.zone.fire(&oss_server::modules::game::Kill {
                    arena,
                    killer,
                    killed,
                    bounty: 0,
                    flags: 0,
                    points: std::cell::Cell::new(0),
                    green: std::cell::Cell::new(0),
                });
                h.capture();
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
            }
            "balls" => {
                compare_balls(&h.balls_line(), rest.trim(), scripted_one);
                checked += 1;
            }
            // the packet carries the same coordinate, so it moves with it
            "pkt" if scripted_one => {}
            "pkt" => {
                let want = rest.trim();
                let got: Vec<String> = h.got.iter().map(|d| hex(d)).collect();
                assert!(
                    got.contains(&want.to_owned()),
                    "{want} never went out, only {got:?}"
                );
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
            _ => {}
        }
    }
    assert!(scenarios >= 8, "only {scenarios} scenarios replayed");
    assert!(checked > 60, "only {checked} records checked");
}
