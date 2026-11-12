//! Game-logic load harness: how much of one core the zone itself (every
//! module of the bundled default zone, without the network transport) needs
//! for N players in one arena.
//!
//! Companion to oss-core's transport harness (`oss-net/examples/soak.rs`):
//! that one measures the transport with a stand-in relay; this one feeds
//! decoded game packets straight into the real zone and drains what it
//! sends, so the two figures add up to the whole server. See
//! `docs/performance.md`.
//!
//! ```sh
//! cargo run --release -p oss-server --example soak -- --peers 500 --seconds 60
//! ```

use std::time::Instant;

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::position::{C2SPosition, WeaponInfo, weapon_type};
use oss_proto::s2c::ChatBody;
use oss_proto::wire::FixedStr;

use oss_server::modules::core as core_mod;
use oss_server::player::{ClientKind, PlayerId, PlayerStatus};
use oss_server::prng::Prng;
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

const HELP: &str = "\
Game-logic load harness for oss-server (no network transport).

  --peers N            players in the arena (default 500)
  --seconds S          measured period in simulated seconds (default 60)
  --warmup S           unmeasured warm-up after everyone joined (default 5)
  --position-ms MS     each player sends a position this often (default 100)
  --weapon-every N     every Nth position carries a bullet (default 5; 0 = none)
  --chat-interval MS   each player sends a public chat this often (default 60000)
  --death-interval MS  each player dies this often (default 30000)
  --churn-ms MS        one player leaves and a new one joins this often (default 1000)
  --spread PX          players move within a square this wide (default 16384)
  --max-playing N      raise General:MaxPlaying for the arena (default: the
                       zone's own value, which is ASSS's default of 100)
  --seed N             random seed (default 1)
  --json               print one JSON line instead of the table
  --max-cpu PCT        exit with status 1 if the zone needs more than PCT % of a core";

#[derive(Debug, Clone)]
struct Opts {
    peers: usize,
    seconds: u64,
    warmup: u64,
    position_ms: u64,
    weapon_every: u64,
    chat_interval_ms: u64,
    death_interval_ms: u64,
    churn_ms: u64,
    spread: i32,
    seed: u64,
    json: bool,
    max_cpu: Option<f64>,
    max_playing: Option<i32>,
}

fn parse() -> Result<Opts, String> {
    let mut o = Opts {
        peers: 500,
        seconds: 60,
        warmup: 5,
        position_ms: 100,
        weapon_every: 5,
        chat_interval_ms: 60_000,
        death_interval_ms: 30_000,
        churn_ms: 1000,
        spread: 16384,
        seed: 1,
        json: false,
        max_cpu: None,
        max_playing: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or(format!("{a} needs a value"));
        let num = |s: String| s.parse::<u64>().map_err(|e| format!("{s}: {e}"));
        match a.as_str() {
            "--peers" => o.peers = num(val()?)? as usize,
            "--seconds" => o.seconds = num(val()?)?,
            "--warmup" => o.warmup = num(val()?)?,
            "--position-ms" => o.position_ms = num(val()?)?.max(10),
            "--weapon-every" => o.weapon_every = num(val()?)?,
            "--chat-interval" => o.chat_interval_ms = num(val()?)?,
            "--death-interval" => o.death_interval_ms = num(val()?)?,
            "--churn-ms" => o.churn_ms = num(val()?)?,
            "--spread" => o.spread = num(val()?)?.clamp(16, 16384) as i32,
            "--seed" => o.seed = num(val()?)?,
            "--json" => o.json = true,
            "--max-cpu" => {
                let v = val()?;
                o.max_cpu = Some(v.parse().map_err(|e| format!("{v}: {e}"))?);
            }
            "--max-playing" => o.max_playing = Some(num(val()?)? as i32),
            "-h" | "--help" => return Err(HELP.into()),
            other => return Err(format!("unknown option {other}\n\n{HELP}")),
        }
    }
    Ok(o)
}

struct Sim {
    pid: PlayerId,
    x: i32,
    y: i32,
    vx: i32,
    vy: i32,
    sent: u64,
    /// tick offsets so players don't all act on the same tick
    phase: u64,
}

#[derive(Default)]
struct Totals {
    zone_ns: u64,
    packets_in: u64,
    packets_out: u64,
    bytes_out: u64,
    ticks: u64,
    tick_max_ns: u64,
    slow_ticks: u64,
}

struct H {
    zone: Zone,
    rng: Prng,
    t: u64,
    measuring: bool,
    tot: Totals,
    buf: Vec<u8>,
    next_name: u32,
}

impl H {
    /// Runs `f` on the zone, timing it when measuring.
    fn timed(&mut self, f: impl FnOnce(&mut Zone)) {
        let start = Instant::now();
        f(&mut self.zone);
        if self.measuring {
            self.tot.zone_ns += start.elapsed().as_nanos() as u64;
        }
    }

    fn packet(&mut self, p: PlayerId, data: &[u8]) {
        if self.measuring {
            self.tot.packets_in += 1;
        }
        let data = data.to_vec();
        self.timed(|z| z.handle_packet(p, &data));
    }

    fn join(&mut self) -> Option<Sim> {
        self.next_name += 1;
        let name = format!("soak{:05}", self.next_name);
        let pid = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None)?;
        self.buf.clear();
        Login {
            kind: c2s::kind::LOGIN,
            flags: 0,
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(b"x"),
            machine_id: self.next_name,
            connect_type: 4,
            timezone_bias: 0,
            unknown1: 0,
            client_version: 134,
            field444: 444,
            field555: 555,
            permission_id: self.next_name,
            reserved: [0; 12],
            cont_id: None,
        }
        .encode(&mut self.buf);
        let b = self.buf.clone();
        self.packet(pid, &b);
        Some(Sim {
            pid,
            x: 0,
            y: 0,
            vx: 0,
            vy: 0,
            sent: 0,
            phase: u64::from(self.rng.below(1000)),
        })
    }

    fn go(&mut self, p: PlayerId) {
        self.buf.clear();
        GoArena {
            ship: 8,
            allow_audio: 0,
            obscenity_filter: 0,
            x_res: 1024,
            y_res: 768,
            arena_type: -3,
            arena: FixedStr::new(b"soak"),
            optional_graphics: None,
        }
        .encode(&mut self.buf);
        let b = self.buf.clone();
        self.packet(p, &b);
    }

    fn tick(&mut self) {
        self.t += 1;
        let t = self.t;
        let start = Instant::now();
        self.zone.set_now(Ticks(t));
        oss_server::timers::run_due(&mut self.zone);
        // what a transport would do with the outbox
        let out = self.zone.take_outbox();
        let (n, bytes) = out.items().iter().fold((0u64, 0u64), |(n, b), i| {
            (n + 1, b + out.bytes(i).len() as u64)
        });
        self.zone.return_outbox(out);
        let ns = start.elapsed().as_nanos() as u64;
        if self.measuring {
            self.tot.zone_ns += ns;
            self.tot.packets_out += n;
            self.tot.bytes_out += bytes;
            self.tot.ticks += 1;
            self.tot.tick_max_ns = self.tot.tick_max_ns.max(ns);
            if ns > 10_000_000 {
                self.tot.slow_ticks += 1;
            }
        }
    }
}

fn main() -> std::process::ExitCode {
    let o = match parse() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::from(2);
        }
    };
    let mut zone = match ServerBuilder::default_zone().build_zone() {
        Ok(z) => z,
        Err(e) => {
            eprintln!("can't build the default zone: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    zone.set_now(Ticks(0));
    let mut h = H {
        zone,
        rng: Prng::new(o.seed),
        t: 0,
        measuring: false,
        tot: Totals::default(),
        buf: Vec::with_capacity(64),
        next_name: 0,
    };
    let pos_every = (o.position_ms / 10).max(1);
    let chat_every = (o.chat_interval_ms / 10).max(1);
    let death_every = (o.death_interval_ms / 10).max(1);
    let churn_every = o.churn_ms / 10;
    let join_ticks = 200u64; // joins spread over 2 s
    let warm_end = join_ticks + 100 + o.warmup * 100;
    let end = warm_end + o.seconds * 100;
    let mut sims: Vec<Sim> = Vec::new();
    let mut pending: Vec<(u64, Sim)> = Vec::new();
    let mut joined = 0usize;
    let place = |rng: &mut Prng, s: &mut Sim, spread: i32| {
        let lo = (16384 - spread) / 2;
        s.x = lo + rng.below(spread as u32) as i32;
        s.y = lo + rng.below(spread as u32) as i32;
        s.vx = rng.below(401) as i32 - 200;
        s.vy = rng.below(401) as i32 - 200;
    };

    let mut capped = false;
    while h.t < end {
        let t = h.t + 1;
        h.measuring = t > warm_end;
        // The arena exists once the first player has entered it. Raising
        // General:MaxPlaying then, before the rest take ships, is what lets
        // every peer be in a ship: ASSS's default cap is 100 per arena.
        if let Some(n) = o.max_playing
            && !capped
            && let Some(id) = h.zone.arenas.find("soak")
            && let Some(a) = h.zone.arenas.get_mut(id)
        {
            let mut c = (*a.cfg).clone();
            c.set("General", "MaxPlaying", &n.to_string());
            a.cfg = std::sync::Arc::new(c);
            capped = true;
        }
        // staggered joins
        while joined < o.peers && (joined as u64) * join_ticks / (o.peers as u64).max(1) <= t {
            if let Some(mut s) = h.join() {
                place(&mut h.rng, &mut s, o.spread);
                pending.push((t + 10, s));
            }
            joined += 1;
        }
        // logged in players go to the arena and take a ship
        let mut i = 0;
        while i < pending.len() {
            if pending[i].0 <= t {
                let (_, s) = pending.swap_remove(i);
                if h.zone.players.get(s.pid).map(|p| p.status) == Some(PlayerStatus::LoggedIn) {
                    h.go(s.pid);
                    pending.push((t + 10, s));
                } else if h.zone.players.get(s.pid).map(|p| p.status) == Some(PlayerStatus::Playing)
                {
                    let ship = h.rng.below(8) as u8;
                    h.packet(s.pid, &[c2s::kind::SET_SHIP, ship]);
                    sims.push(s);
                } else {
                    pending.push((t + 5, s));
                }
            } else {
                i += 1;
            }
        }
        // churn
        if churn_every > 0 && t > join_ticks && t.is_multiple_of(churn_every) && !sims.is_empty() {
            let k = h.rng.below(sims.len() as u32) as usize;
            let gone = sims.swap_remove(k);
            h.timed(|z| core_mod::kick(z, gone.pid));
            if let Some(mut s) = h.join() {
                place(&mut h.rng, &mut s, o.spread);
                pending.push((t + 10, s));
            }
        }
        // traffic
        for k in 0..sims.len() {
            let phase = sims[k].phase;
            if (t + phase).is_multiple_of(pos_every) {
                let (pid, x, y, vx, vy, n) = {
                    let s = &mut sims[k];
                    s.x += s.vx * pos_every as i32 / 100;
                    s.y += s.vy * pos_every as i32 / 100;
                    let lo = (16384 - o.spread) / 2;
                    let hi = lo + o.spread - 1;
                    if s.x < lo || s.x > hi {
                        s.vx = -s.vx;
                        s.x = s.x.clamp(lo, hi);
                    }
                    if s.y < lo || s.y > hi {
                        s.vy = -s.vy;
                        s.y = s.y.clamp(lo, hi);
                    }
                    s.sent += 1;
                    (s.pid, s.x, s.y, s.vx, s.vy, s.sent)
                };
                let mut w = WeaponInfo::default();
                if o.weapon_every > 0 && n % o.weapon_every == 0 {
                    w.set_kind(weapon_type::BULLET);
                }
                h.buf.clear();
                C2SPosition {
                    rotation: (n % 40) as i8,
                    time: Ticks(t).wire(),
                    x_speed: vx as i16,
                    y_speed: vy as i16,
                    x: x as i16,
                    y: y as i16,
                    bounty: 10,
                    energy: 1000,
                    weapon: w,
                    ..Default::default()
                }
                .encode(&mut h.buf);
                let b = h.buf.clone();
                h.packet(pid, &b);
            }
            if (t + phase * 7).is_multiple_of(chat_every) {
                h.buf.clear();
                c2s::C2SChat(ChatBody {
                    kind: chat_type::PUBLIC,
                    sound: 0,
                    pid: 0,
                    text: b"soak test chatter".to_vec(),
                })
                .encode(&mut h.buf);
                let b = h.buf.clone();
                let pid = sims[k].pid;
                h.packet(pid, &b);
            }
            if (t + phase * 13).is_multiple_of(death_every) && sims.len() > 1 {
                let killer =
                    sims[(k + 1 + h.rng.below(sims.len() as u32 - 1) as usize) % sims.len()].pid;
                h.buf.clear();
                c2s::Death {
                    killer: killer.0 as i16,
                    bounty: 10,
                }
                .encode(&mut h.buf);
                let b = h.buf.clone();
                let pid = sims[k].pid;
                h.packet(pid, &b);
            }
        }
        h.tick();
    }

    let secs = o.seconds as f64;
    let cpu = 100.0 * h.tot.zone_ns as f64 / 1e9 / secs;
    let pkts = (h.tot.packets_in + h.tot.packets_out).max(1);
    let in_arena = h
        .zone
        .players
        .iter()
        .filter(|p| p.status == PlayerStatus::Playing)
        .count();
    let in_ships = h
        .zone
        .players
        .iter()
        .filter(|p| p.status == PlayerStatus::Playing && !p.ship.is_spec())
        .count();
    if o.json {
        println!(
            "{{\"peers\":{},\"seconds\":{},\"zone_cpu_percent\":{cpu:.3},\"ns_per_packet\":{:.1},\"packets_in_per_s\":{:.0},\"packets_out_per_s\":{:.0},\"bytes_out_per_s\":{:.0},\"tick_max_ms\":{:.3},\"slow_ticks\":{},\"playing_at_end\":{in_arena},\"in_ships_at_end\":{in_ships}}}",
            o.peers,
            o.seconds,
            h.tot.zone_ns as f64 / pkts as f64,
            h.tot.packets_in as f64 / secs,
            h.tot.packets_out as f64 / secs,
            h.tot.bytes_out as f64 / secs,
            h.tot.tick_max_ns as f64 / 1e6,
            h.tot.slow_ticks,
        );
    } else {
        println!(
            "oss-server game-logic soak: {} players, {} s measured (seed {})",
            o.peers, o.seconds, o.seed
        );
        println!();
        println!("zone CPU              {cpu:>10.2} % of one core");
        println!(
            "per game packet       {:>10.1} ns (in + out)",
            h.tot.zone_ns as f64 / pkts as f64
        );
        println!(
            "game packets in       {:>10.0} /s",
            h.tot.packets_in as f64 / secs
        );
        println!(
            "game packets out      {:>10.0} /s ({:.1} Mbit/s before transport framing)",
            h.tot.packets_out as f64 / secs,
            h.tot.bytes_out as f64 * 8.0 / secs / 1e6
        );
        println!(
            "longest tick          {:>10.3} ms ({} over 10 ms)",
            h.tot.tick_max_ns as f64 / 1e6,
            h.tot.slow_ticks
        );
        println!("playing at the end    {in_arena:>10}");
        println!("in a ship at the end  {in_ships:>10}");
    }
    match o.max_cpu {
        Some(max) if cpu > max => {
            eprintln!("FAILED: zone CPU {cpu:.2}% is over {max}%");
            std::process::ExitCode::FAILURE
        }
        _ => std::process::ExitCode::SUCCESS,
    }
}
