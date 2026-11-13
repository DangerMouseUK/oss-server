# Game-logic performance: the soak harness

Harness: `crates/oss-server/examples/soak.rs`.

The project's budget is **500 players in one arena using under 20% of one CPU core**. oss-core's transport harness (`oss-net/examples/soak.rs`, documented in oss-core's `docs/performance.md`) measures the transport's share with a stand-in relay. This harness measures the other part: the zone itself, meaning every module of the bundled default zone running real game logic, without the network transport. The two figures add up to the whole server.

## Running it

```sh
cargo run --release -p oss-server --example soak -- --peers 500 --seconds 60
cargo run --release -p oss-server --example soak -- --peers 500 --seconds 60 --json
cargo run --release -p oss-server --example soak -- --help
```

Always build with `--release`. With `--max-cpu PCT` the harness exits with status 1 if the zone needs more than that share of a core. CI's `soak` job runs `--peers 500 --seconds 30 --max-playing 500 --max-cpu 15`.

**`--max-playing` is needed for the budget scenario, and this is the trap.** ASSS's `fm_normal` caps a single arena at `General:MaxPlaying` players, default **100**, and the bundled sample zone does not raise it. A spectator's position is never relayed (`relay()` runs only for players in a ship), so a plain `--peers 500` run measures 100 players relaying and 400 spectators who send positions that go nowhere: 105,125 packets out per second and 1.6% of a core, about a quarter of the real load. `--max-playing 500` raises the cap for the soak arena so all 500 are in ships, which is what the PRD's "500 players in one arena" means. The zone's own `[Misc] MaxPlaying=75` is not that setting; `fm_normal` reads `General:MaxPlaying`, and ASSS ignores the `[Misc]` line too.

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
| `--max-playing` | unset | raises `General:MaxPlaying` for the soak arena; unset leaves ASSS's default of 100 |

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

**Allocations and heap.** How many allocations the zone makes per second, and how much heap it is holding at the end, per player. See [Allocations](#allocations).

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

**These rows need `--max-playing` to reproduce today.** They were taken before `General:MaxPlaying` was implemented (`Complete the freq manager against fm_normal.c`), when every player got a ship whatever the cap said. Re-measured on 2026-11-12 on the same machine with `--max-playing 500`, the traffic matches to within a handful of packets — 412,514 against 412,516 packets out per second at 500 players — which is what confirms the two runs describe the same scenario. CPU came out at 6.7% against 6.8%, i.e. unchanged, once the measurement was paired with a control and taken on the quieter machine (see [Measuring under load](#measuring-under-load)). Earlier unpaired runs in this session read 7.4–8.3%; that was the machine, not the zone. A plain `--peers 500` today gives 1.6% and 105,125 packets out: 100 in ships, 400 in spectator mode.

**Crowding** (500 players), 2026-11-08 figures with the 2026-11-12 `--max-playing 500` re-measurement beside them:

| Area | Zone CPU (2026-11-08) | Zone CPU (2026-11-12) | busy / quiet | Packets out per s | Recipients per position |
|---|---|---|---|---|---|
| whole map (16384 px) | 6.8% | 6.7% | 7.00 / 6.71 | 412,514 | about 82 |
| half the map (8192 px) | 6.6% | 6.4% | 6.90 / 6.36 | 1,061,253 | about 210 |
| a quarter (4096 px) | 8.1% | 7.5% | 8.05 / 7.51 | 1,776,405 | about 350 |

Each 2026-11-12 figure is the median of three runs paired with controls, taken twice: with the machine at 99% busy and again at 51% (see [Measuring under load](#measuring-under-load)). The quiet set is the one quoted, and it is remarkably tight — the three whole-map runs spanned 6.69 to 6.74. They reproduce the 2026-11-08 figures: **the zone has not got more expensive** since, despite the modules added in between.

The zone's own cost hardly changes with crowding. The number of packets it hands the transport does.

## Against the 20% budget

Measured, not computed, on 2026-11-12. The transport half comes from oss-core's harness run at the fan-outs this harness produces (`--fanout 82`, `210`, `350`), the zone half from this one at the matching `--spread` with `--max-playing 500`. Both halves on the same machine, VIE, 500 players, each figure the median of three runs paired with a control (see [Measuring under load](#measuring-under-load)).

| Arena | Zone | Transport (VIE) | **Whole server** | Fits 20%? |
|---|---|---|---|---|
| whole map, fan-out 82 | 6.7% | 5.3% | **about 12%** | yes |
| half the map, fan-out 210 | 6.4% | 11.6% | **about 18%** | only just |
| a quarter, fan-out 350 | 7.5% | 19.6% | **about 27%** | **no** |

With OpenSec rather than VIE the transport costs 8.5%, 16.0% and 27.5%, giving about 15%, 22% and 35% for the whole server: the budget then fails from half a map onward.

Every figure was taken under two load conditions, 99% busy and 51%, and the two agree to within 2%, so these totals are not artefacts of a shared machine.

**The earlier estimate of ~32% for the transport alone was pessimistic.** It multiplied 1.8M packets/s by the fan-out-30 cost of 178 ns per packet. Measured, per-packet cost *falls* with volume — about 180 ns at fan-out 30, 125 ns at 82, 110 ns at 210 — because clustering packs more packets into each datagram: from fan-out 30 to 82 the packet rate rises 2.7 times while the datagram rate rises 4%. Clustering saturates near 20 packets per datagram, so beyond fan-out 210 the cost per packet stops falling. The real transport figure for a quarter-map arena is 19.6%, not 32% — which is still the whole budget on its own.

**The budget still does not close.** Being wrong about the magnitude does not change the verdict: a crowded arena of 500 players in ships needs about 27% of a core with VIE and about 35% with OpenSec. Half a map sits at about 18%, inside 20% but with less margin than the difference between the two ciphers. Only the uncrowded whole-map case has real margin.

These volumes are what the ASSS relay rules produce. ASSS would send the same packets, so reducing them would mean changing relay behaviour, for example a smaller radar share in crowded arenas. That is a design decision, not an optimisation.

Two things that would move the number before any such decision, both located but not acted on here:

- ~~**The transport allocates on the hot path at these volumes**~~ — **both halves have since been measured and fixed, and the "part of the 20% is that allocation traffic" guess was wrong.** oss-core cut the transport from 28,348 allocations/s at fan-out 350 to 189/s (`CONN_REFILL`, not the shared pool everyone suspected) and measured no CPU change outside the noise. The zone half is below under [Allocations](#allocations): 75,045/s to 2,071/s, again with the CPU change inside the run-to-run spread. **The 20% budget verdict above is unchanged by either fix.**
- **Tick headroom is fine; an earlier claim here that it was not was wrong.** Unpaired runs recorded a 9.9 ms tick and a single 10.9 ms one against a 10 ms budget. On the quieter machine, nine paired runs across all three crowding levels peaked at 1.5–1.7 ms with no tick over 10 ms at all. Those long ticks were the loaded machine descheduling the harness, not zone work. The longest tick is about a sixth of the budget at 500 players in ships, with the transport's share still to be added.

## Allocations

The harness counts every allocation made while the zone is running: a thread-local flag is set around `Zone::handle_packet` and around the timer-and-drain part of each tick, and the global allocator counts only what happens inside it. The simulated clients and the harness's own bookkeeping are outside it. `--json` reports `zone_allocs_per_s`, `zone_heap_bytes` and `heap_per_peer`; the table prints the same. This is the same shape as oss-core's transport harness, so the two sets of figures mean the same thing and can be added.

Unlike oss-core's, these counts are **not** exactly deterministic — the zone's own RNG is unseeded, so repeated runs differ by about 0.1% (57,264 / 57,224 / 57,211 for the same arguments). That is far below any difference worth reporting, but a 1% claim would need repeats.

Measured 2026-11-13, 500 players, `--max-playing 500`, 10 s:

| Arena | before | after |
|---|---|---|
| whole map (16384 px) | 57,211 /s | 2,071 /s |
| half the map (8192 px) | 67,443 /s | 2,071 /s |
| a quarter (4096 px) | 75,045 /s | 2,071 /s |
| an eighth (2048 px) | 79,281 /s | 2,071 /s |
| a sixteenth (1024 px) | 80,173 /s | 2,071 /s |

**The zone did not have the transport's defect.** oss-core's was a per-tick demand that scales with fan-out met from a constant-sized reserve, so its rate rose 190-fold from fan-out 30 to 350. The zone's rate rose 1.4-fold across the same crowding while the packets it relayed rose six-fold: it was flat in fan-out and linear in **inbound** packets — about 12 to 15 allocations for every position that arrived, whoever it was relayed to. Halving `--position-ms` doubled it (40,134 / 75,053 / 144,853 at 200, 100 and 50 ms); crowding barely moved it.

Sampled stacks (one allocation in 997) put 92.5% of it in two places, both in `game::relay`, which runs once per position or weapon packet:

- **66.2%** — the three recipient lists, one per position tail, built from nothing for every packet and dropped again. The size histogram is the giveaway: a doubling series 8, 16, 32 ... 512 bytes with monotonically falling counts, which is one `Vec` growing by `push` from empty.
- **26.3%** — `net_cfg` reading four `Net:` settings per packet through a `ConfigFile::get` that built its `Section:Key` lookup string on the heap. `Net:PositionExtraPixels` is 23 characters, and 23-byte allocations were 13.6% of the total on their own.

Both are fixed: the lists and the packet buffer live in a `RelayScratch` taken from the registry for the call and put back emptied, so each keeps the capacity its arena's fan-out has needed, and `ConfigFile::get` builds the key in a stack buffer, falling back to the heap only for keys over 96 bytes. `tests/alloc_free_relay.rs` guards it, and it separates the two: for 4,000 relayed positions through 40 players it counts **47,600** allocations on the parent, **31,600** with only the config fix, **16,000** with only the scratch fix, and **0** with both. The 4.0 per packet the config fix removes is exactly the four `Net:` lookups.

CPU, three interleaved pairs at a quarter-map spread, since allocation counts need no control but CPU does: 7.81 / 7.68 / 7.65% before against 7.69 / 7.08 / 7.14% after. The medians differ by about 5% but the ranges overlap, so **the honest reading is that removing 73,000 allocations a second changed CPU by less than this machine's run-to-run spread** — the same result oss-core got. Heap per player went from 8,908 to 8,910 bytes, so the reuse costs nothing to hold.

What is left, 2,071/s, is almost all the arena-entry path: `send_arena_response` builds a latin1 copy of every name for the player list, so a join into a 500-player arena costs about 500 allocations. The default `--churn-ms 1000` makes one join a second. It scales with joins, not with traffic, which is why the figure is identical at every crowding and every position rate — and it is the reason the number is 2,071 rather than nearly zero.

## Measuring under load

The 2026-11-12 figures were taken on a shared desktop running an unrelated six-thread job (about 5.85 of 8 logical cores) plus two other build-and-test sessions, with the machine reporting 99% busy. That is disclosed rather than hidden because it changes how to read them.

Each measurement was paired with an immediately preceding `oss-net` soak run at its default `--fanout 30`, used as a load probe: that harness is deterministic, so its traffic is fixed and only its timing moves, and its clean value (2.8%) is recorded in oss-core's `docs/performance.md`. The medians above are of three such runs each.

This mattered. In one interleaved sequence a control returned 5.55% against neighbours at 2.92–2.96%, a burst landing inside a single 60-second run — and both claims this session had to withdraw (a zone cost of 7.4–8.3%, and a near-overrunning tick) came from unpaired runs. A single control at the start of a session does not protect a measurement taken twenty minutes later.

The whole set was then repeated after two competing jobs stopped, with the machine at 51% busy instead of 99%. Every figure moved by under 8%, the transport figures by under 2%, and the control itself by 0.03 points. The load does not apply a steady tax to a single-threaded harness; it occasionally swallows a whole run, which is what pairing catches. Memory deserves a glance as well as CPU: this box has 18 GB, and a sibling job earlier in the day reached a 9.8 GB working set, which would contend for bandwidth in a way a CI runner never does.

**Figures to rely on should still come from CI**, where the runner is clean; that is what the `soak` jobs in both repos now produce on every push. The numbers here are good to a few percent — enough to locate the cliff and set a gate, not enough to argue about a point either way.

## What is gated

CI runs `--peers 500 --seconds 30 --max-playing 500 --max-cpu 15` as the `soak` job: the PRD scenario, with the threshold set from the measured 7.0% (8.3% worst observed under load) plus headroom for a shared CI runner, and still comfortably under the 20% whole-server budget. oss-core gates its half at `--fanout 30` (the old smoke test) and `--fanout 82 --max-cpu 12` (the whole-map case). The crowded case is deliberately left ungated in both repos, because it does not pass; see the table above.

## Reporting, not fixed here

- **The PRD states the budget at 60 Hz** (`PRD.md` line 108). The server ticks at **100 Hz** (`crates/oss-server/src/runtime.rs:190`), which all three repos' `CLAUDE.md` give as SubSpace's timebase. Every figure here is measured at 100 Hz, so they answer the harder reading. If the PRD line is meant literally, the budget is 1.67 times easier than what is measured here; if it is stale, the numbers above stand as they are. That line is the lead's to settle.
- **The requirement has no row in `notes/parity.md` and no entry in `notes/verification-plan.md`.** Suggested wording for the parity row: *"500 players in one arena under 20% of one core (PRD line 108) — zone and transport measured separately by the two soak harnesses; gated in CI by `oss-server` `soak` (`--max-playing 500 --max-cpu 15`) and `oss-core` `soak` (`--fanout 82 --max-cpu 12`). Open: a crowded arena (fan-out 350) measures about 27% and does not meet the budget."*

## Change found by the harness

The first 500-player run measured 15.3% of a core for the zone.

**Cause.** Module data per player and per arena lives in maps keyed by type (`Resources` and `Extensions`), and those maps used Rust's default SipHash. The position relay looks up the recipient's game data for every candidate recipient of every position, about 2.5 million lookups a second at 500 players.

**Fix.** A `TypeId` is already a hash, so the maps now pass it straight through. The zone dropped to 6.8% (163 ns per packet instead of 364) with identical traffic. The change applies to every module's type-keyed data lookups.
