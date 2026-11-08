# Game-logic performance: the soak harness

Harness: `crates/oss-server/examples/soak.rs`.

The project's budget is **500 players in one arena using under 20% of one CPU core**. oss-core's transport harness (`oss-net/examples/soak.rs`, documented in oss-core's `docs/performance.md`) measures the transport's share with a stand-in relay. This harness measures the other part: the zone itself, meaning every module of the bundled default zone running real game logic, without the network transport. The two figures add up to the whole server.

## Running it

```sh
cargo run --release -p oss-server --example soak -- --peers 500 --seconds 60
cargo run --release -p oss-server --example soak -- --peers 500 --seconds 60 --json
cargo run --release -p oss-server --example soak -- --help
```

Always build with `--release`. With `--max-cpu PCT` the harness exits with status 1 if the zone needs more than that share of a core.

## The scenario

Everything runs in one thread on a simulated clock (100 ticks a second, as in the real game loop). Simulated players are game-protocol clients without a network connection. Their packets go straight to `Zone::handle_packet`, and what the zone queues for them is drained each tick the way a transport would drain it.

| Option | Default | Meaning |
|---|---|---|
| `--peers` | 500 | players; they log in over the first 2 s, enter one arena and take a random ship |
| `--seconds` / `--warmup` | 60 / 5 | measured period, after an unmeasured warm-up |
| `--position-ms` | 100 | each player sends a 22-byte position this often, moving in a straight line and bouncing off the edges of the area |
| `--weapon-every` | 5 | every 5th position carries a bullet |
| `--chat-interval` | 60000 | each player sends a public chat this often |
| `--death-interval` | 30000 | each player is killed by another this often (kill points, bounty, prizes) |
| `--churn-ms` | 1000 | every interval one player is kicked and a new one joins |
| `--spread` | 16384 | players move within a square this many pixels wide (16384 is the whole map) |
| `--seed` | 1 | random seed for the simulated players |

Unlike the transport harness, relay fan-out is not a parameter. The zone decides who receives each position, as ASSS does:
- players within the recipient's screen size;
- a distance-weighted random share of radar updates out to `Net:PositionExtraPixels`;
- weapons within `Net:BulletPixels` or `Net:WeaponPixels`.

The fan-out therefore follows from how crowded the arena is, which `--spread` controls.

## What is measured

**Zone CPU.** This is the wall-clock time spent in the zone, divided by the simulated duration: the fraction of one core the game logic needs. The timed work is:
- `Zone::handle_packet` for every incoming game packet, which runs the module handlers;
- `timers::run_due` every tick (lagaction, security checks, flag and ball timers, score updates, persistence, ...);
- draining the outbox.

The simulated clients are not timed.

**Cost per game packet.** Zone time divided by game packets in plus out.

**Longest tick.** The slowest single tick's timers plus drain, and how many ticks went over the 10 ms tick length.

Packet counts vary by a few hundred per second between runs because the zone's own random number generator (radar sampling, prizes) isn't seeded by the harness.

## Results

Measured on 2026-11-08: Intel Core i9-7980XE at 2.6 GHz, Windows 11, Rust 1.99.1, release build, default options. This is the same machine model as oss-core's transport figures. Repeated runs vary by about 10% in CPU time.

| Players | Zone CPU (% of one core) | ns per game packet | Packets in / out per s | Out (Mbit/s, before framing) | Longest tick |
|---|---|---|---|---|---|
| 100 | 0.4 | 236 | 1,006 / 16,695 | 2.3 | 0.5 ms |
| 250 | 1.9 | 183 | 2,513 / 101,938 | 14.0 | 0.6 ms |
| **500** | **6.8** | 163 | 5,026 / 412,516 | 63.8 | 1.6 ms |
| 1,000 | 25.3 | 154 | 10,051 / 1,638,198 | 267 | 3.6 ms |

Every player was still playing at the end, apart from the one in the middle of churn, and no tick came near 10 ms.

**Crowding** (500 players):

| Area | Zone CPU | Packets out per s | Recipients per position |
|---|---|---|---|
| whole map (16384 px) | 6.8% | 412,516 | about 82 |
| half the map (8192 px) | 6.6% | 1,061,973 | about 210 |
| a quarter (4096 px) | 8.1% | 1,777,532 | about 350 |

The zone's own cost hardly changes with crowding. The number of packets it hands the transport does.

## Against the 20% budget

The zone alone takes **about 7% of a core at 500 players**, so game logic fits.

The open question is the transport. oss-core measured 2.8% for VIE at 500 players (178 ns per game packet), with a fan-out of 30, which is about 151,000 packets out per second. With ASSS's real relay rules the server hands the transport far more:
- **Whole map:** about 412,000 packets per second. At oss-core's per-packet cost that is roughly 7% of a core, so about 14% for the whole server.
- **Quarter of the map:** about 1.8 million packets per second. That would be roughly 32% for the transport alone, well over the budget.

How the per-packet cost scales at those volumes is untested, since clustering many small packets into fewer datagrams should help. The next measurement is oss-core's harness with `--fanout 82` and `--fanout 350`, or a full-stack run of this harness through the real transport.

These volumes are what the ASSS relay rules produce. ASSS would send the same packets, so reducing them would mean changing relay behaviour, for example a smaller radar share in crowded arenas. That is a design decision, not an optimisation.

## Change found by the harness

The first 500-player run measured 15.3% of a core for the zone.

**Cause.** Module data per player and per arena lives in maps keyed by type (`Resources` and `Extensions`), and those maps used Rust's default SipHash. The position relay looks up the recipient's game data for every candidate recipient of every position, about 2.5 million lookups a second at 500 players.

**Fix.** A `TypeId` is already a hash, so the maps now pass it straight through. The zone dropped to 6.8% (163 ns per packet instead of 364) with identical traffic. The change applies to every module's type-keyed data lookups.
