//! The position relay allocates nothing once an arena is warm.
//!
//! `game::relay` runs for every position and weapon packet, so anything it
//! allocates is multiplied by the whole arena's packet rate. It used to build
//! three recipient lists and a packet buffer from nothing each time, and to
//! read four `Net:` settings through a `ConfigFile::get` that built its lookup
//! key on the heap: about 15 allocations per inbound packet, 75,000 a second
//! at 500 players (`docs/performance.md`).
//!
//! This test pushes positions through a warm arena and asserts **zero**
//! allocations. It fails on the parent of the commit that added it, with one
//! allocation for roughly every fifteenth counted byte of work; the older
//! `tests/` flow tests could not have caught it, because none of them sends
//! enough positions for the cost to show.

// The counter is a global allocator wrapper: `unsafe` only to forward to
// `System`.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena};
use oss_proto::position::{C2SPosition, WeaponInfo};
use oss_proto::wire::FixedStr;

use oss_server::modules::core as core_mod;
use oss_server::player::{ClientKind, PlayerId, PlayerStatus};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}
static ALLOCS: AtomicU64 = AtomicU64::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn counted<T>(f: impl FnOnce() -> T) -> T {
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    r
}

/// Players in the arena. Under ASSS's default `General:MaxPlaying` of 100, so
/// every one of them is in a ship and every position is relayed to the rest.
const PLAYERS: usize = 40;
/// Rounds (one position from every player) pushed through before counting
/// starts, to grow every buffer the steady state needs.
const WARM_ROUNDS: usize = 100;
/// Rounds counted.
const COUNTED_ROUNDS: usize = 100;

fn login(zone: &mut Zone, n: u32) -> PlayerId {
    let name = format!("alloc{n:04}");
    let p = core_mod::new_connection(zone, ClientKind::Vie, None).expect("slot");
    let mut buf = Vec::new();
    c2s::Login {
        kind: c2s::kind::LOGIN,
        flags: 0,
        name: FixedStr::new(name.as_bytes()),
        password: FixedStr::new(b"x"),
        machine_id: n,
        connect_type: 4,
        timezone_bias: 0,
        unknown1: 0,
        client_version: 134,
        field444: 444,
        bound_ip: 0,
        permission_id: n,
        server_ip: 0,
        bound_port: 0,
        redirect_id: 0,
        using_proxy: 0,
        unused: 0,
        cont_id: None,
    }
    .encode(&mut buf);
    zone.handle_packet(p, &buf);
    p
}

fn go(zone: &mut Zone, p: PlayerId) {
    let mut buf = Vec::new();
    GoArena {
        ship: 8,
        allow_audio: 0,
        obscenity_filter: 0,
        x_res: 1024,
        y_res: 768,
        arena_type: -3,
        arena: FixedStr::new(b"alloc"),
        optional_graphics: None,
    }
    .encode(&mut buf);
    zone.handle_packet(p, &buf);
}

fn position(x: i16, y: i16, time: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    C2SPosition {
        rotation: 10,
        time,
        x_speed: 100,
        y,
        checksum: 0,
        status: 0,
        x,
        y_speed: 100,
        bounty: 7,
        energy: 1000,
        weapon: WeaponInfo::default(),
        extra: None,
    }
    .encode(&mut buf);
    buf
}

#[test]
fn the_position_relay_allocates_nothing_when_warm() {
    let mut zone = ServerBuilder::default_zone().build_zone().expect("zone");
    let mut t = 0u64;
    // drains the outbox the way a transport would, and says how much it took
    let tick = |z: &mut Zone, t: &mut u64, n: u64| -> usize {
        let mut drained = 0;
        for _ in 0..n {
            *t += 1;
            z.set_now(Ticks(*t));
            oss_server::timers::run_due(z);
            let out = z.take_outbox();
            drained += out.items().len();
            z.return_outbox(out);
        }
        drained
    };

    // everyone logs in, enters the arena and takes a ship
    let mut players = Vec::new();
    for n in 0..PLAYERS as u32 {
        players.push(login(&mut zone, n));
    }
    tick(&mut zone, &mut t, 20);
    for &p in &players {
        go(&mut zone, p);
    }
    tick(&mut zone, &mut t, 60);
    for &p in &players {
        zone.handle_packet(p, &[c2s::kind::SET_SHIP, 1]);
    }
    tick(&mut zone, &mut t, 20);
    let playing = zone
        .players
        .iter()
        .filter(|p| p.status == PlayerStatus::Playing && !p.ship.is_spec())
        .count();
    assert_eq!(playing, PLAYERS, "every player should be in a ship");

    // Spread them over a small area so each position is relayed to the rest.
    // Every packet carries a later `time` than the last from that player:
    // the zone drops a position that is not newer than the one before it, so
    // reusing one packet would measure nothing. They are all built up front,
    // because building one allocates and only the zone's work is counted.
    let rounds = WARM_ROUNDS + COUNTED_ROUNDS;
    let packets: Vec<Vec<Vec<u8>>> = (0..rounds)
        .map(|r| {
            (0..PLAYERS)
                .map(|i| {
                    position(
                        8000 + (i as i16) * 13,
                        8000 + (i as i16) * 7,
                        r as u32 * 10 + 1,
                    )
                })
                .collect()
        })
        .collect();
    let mut relayed = 0;
    for round in &packets[..WARM_ROUNDS] {
        for (i, pkt) in round.iter().enumerate() {
            zone.handle_packet(players[i], pkt);
        }
        relayed += tick(&mut zone, &mut t, 1);
    }
    // the warm-up has to have relayed to everyone, or this test proves nothing
    // all but the first couple of rounds, where not everyone has a position yet
    let want = (WARM_ROUNDS - 3) * PLAYERS * (PLAYERS - 1);
    assert!(
        relayed >= want,
        "the warm-up relayed {relayed} packets, expected at least {want}"
    );

    let before = ALLOCS.load(Ordering::Relaxed);
    counted(|| {
        for round in &packets[WARM_ROUNDS..] {
            for (i, pkt) in round.iter().enumerate() {
                zone.handle_packet(players[i], pkt);
            }
            // drain as a transport would; left to pile up, the outbox's own
            // arena would double a few times and that is not what is measured
            let out = zone.take_outbox();
            zone.return_outbox(out);
        }
    });
    let allocs = ALLOCS.load(Ordering::Relaxed) - before;
    assert_eq!(
        allocs,
        0,
        "{} relayed positions allocated {allocs} times",
        COUNTED_ROUNDS * PLAYERS
    );
}
