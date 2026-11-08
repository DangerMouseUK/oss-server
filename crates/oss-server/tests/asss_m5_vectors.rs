//! M5 game-rule vectors generated from unmodified ASSS 1.4.4 code (see
//! `fixtures/asss-vectors/`): brick placement and the empty-tile spiral over
//! synthetic maps regenerated here byte for byte, weighted prizes, kill
//! points, periodic rewards and the warzone flag reward.

use oss_server::clientset::PrizeWeights;
use oss_server::modules::bricks::{BrickMode, find_brick_endpoints};
use oss_server::modules::jackpot::warzone_reward;
use oss_server::modules::mapdata::ArenaMap;
use oss_server::modules::periodic::periodic_points;
use oss_server::modules::stats::{KillSettings, kill_points};

const BRICKS: &str = include_str!("fixtures/asss-vectors/bricks.txt");
const EMPTY: &str = include_str!("fixtures/asss-vectors/empty-tile.txt");
const PRIZES: &str = include_str!("fixtures/asss-vectors/prize-weights.txt");
const KILLS: &str = include_str!("fixtures/asss-vectors/kill-points.txt");
const PERIODIC: &str = include_str!("fixtures/asss-vectors/periodic-reward.txt");
const FLAGS: &str = include_str!("fixtures/asss-vectors/flag-points.txt");

struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn rec(out: &mut Vec<u8>, x: u32, y: u32, t: u32) {
    out.extend_from_slice(&(x | (y << 12) | (t << 24)).to_le_bytes());
}

fn size(t: u32) -> u32 {
    match t {
        217 => 2,
        219 => 6,
        220 => 5,
        _ => 1,
    }
}

/// `gen_synth_maps.py` `place()`.
fn place(
    rng: &mut SplitMix,
    occ: &mut [bool],
    out: &mut Vec<u8>,
    count: usize,
    ylo: u32,
    yspan: u32,
) {
    let mut accepted = 0;
    while accepted < count {
        let x = rng.below(1024) as u32;
        let y = ylo + rng.below(u64::from(yspan)) as u32;
        let t = 1 + rng.below(220) as u32;
        let s = size(t);
        if x + s > 1024 || y + s > 1024 {
            continue;
        }
        let cells =
            || (y..y + s).flat_map(move |cy| (x..x + s).map(move |cx| (cy * 1024 + cx) as usize));
        if cells().any(|i| occ[i]) {
            continue;
        }
        cells().for_each(|i| occ[i] = true);
        rec(out, x, y, t);
        accepted += 1;
    }
}

fn synth_a() -> Vec<u8> {
    let mut rng = SplitMix(0x5EED_0001);
    let mut occ = vec![false; 1024 * 1024];
    let mut out = Vec::new();
    place(&mut rng, &mut occ, &mut out, 3000, 0, 1024);
    assert_eq!(out.len(), 12000);
    out
}

fn synth_b() -> Vec<u8> {
    let mut out = Vec::new();
    for x in 0..1024 {
        rec(&mut out, x, 0, 1 + (7 * x) % 216);
    }
    for x in 0..1024 {
        rec(&mut out, x, 1023, 1 + (5 * x) % 216);
    }
    for y in 1..1023 {
        rec(&mut out, 0, y, 1 + (11 * y) % 216);
    }
    for y in 1..1023 {
        rec(&mut out, 1023, y, 1 + (13 * y) % 216);
    }
    for i in 1..=171 {
        rec(&mut out, i + 100, i + 100, i);
    }
    rec(&mut out, 1017, 500, 219);
    rec(&mut out, 1, 1, 220);
    rec(&mut out, 1021, 1021, 217);
    rec(&mut out, 1024, 10, 1);
    rec(&mut out, 4095, 4095, 255);
    out
}

fn map_named(maps: &[(&str, ArenaMap)], name: &str) -> ArenaMap {
    maps.iter()
        .find(|(n, _)| *n == name)
        .map(|(_, m)| m.clone())
        .expect("known map")
}

fn maps() -> Vec<(&'static str, ArenaMap)> {
    vec![
        ("synth-a-plain.lvl", ArenaMap::from_bytes(&synth_a())),
        ("synth-b-edges.lvl", ArenaMap::from_bytes(&synth_b())),
    ]
}

fn quad(s: &str) -> (i32, i32, i32, i32) {
    let v: Vec<i32> = s.split(',').map(|x| x.parse().expect("int")).collect();
    (v[0], v[1], v[2], v[3])
}

#[test]
fn bricks_match_asss() {
    let maps = maps();
    let mut n = 0;
    for line in BRICKS
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        let map = map_named(&maps, f[1]);
        let mode = match f[2] {
            "0" => BrickMode::Vie,
            "1" => BrickMode::Ahead,
            "2" => BrickMode::Lateral,
            _ => BrickMode::Cage,
        };
        let p = |i: usize| f[i].parse::<i32>().expect("int");
        let coin: u32 = f[7].parse().expect("coin");
        let got = find_brick_endpoints(&map, mode, p(3), p(4), p(5), p(6), coin);
        let want: Option<Vec<(i32, i32, i32, i32)>> = if f[8] == "blocked" {
            None
        } else {
            Some(f[8..].iter().map(|s| quad(s)).collect())
        };
        assert_eq!(got, want, "{line}");
        n += 1;
    }
    assert_eq!(n, 3000);
}

#[test]
fn empty_tile_spiral_matches_asss() {
    let maps = maps();
    let mut n = 0;
    for line in EMPTY
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        let map = map_named(&maps, f[1]);
        let p = |i: usize| f[i].parse::<i32>().expect("int");
        assert_eq!(map.find_empty_tile_near(p(2), p(3)), (p(4), p(5)), "{line}");
        n += 1;
    }
    assert!(n > 300);
}

#[test]
fn prizes_match_asss() {
    let mut weights = [0u8; 28];
    let mut pw = None;
    let mut n = 0;
    for line in PRIZES
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f[0] {
            "weights" => {
                for (w, s) in weights.iter_mut().zip(&f[1..]) {
                    *w = s.parse().expect("weight");
                }
                pw = Some(PrizeWeights::from_weights(&weights));
            }
            "sums" => {
                let want: Vec<u16> = f[1..].iter().map(|s| s.parse().expect("sum")).collect();
                assert_eq!(&pw.as_ref().expect("weights first").sums()[..], &want[..]);
            }
            "prize" => {
                let r: u32 = f[1].parse().expect("r");
                let want: u8 = f[2].parse().expect("prize");
                assert_eq!(
                    pw.as_ref().expect("weights").random_prize(|_| r),
                    want,
                    "{line}"
                );
                n += 1;
            }
            _ => {}
        }
    }
    assert!(n > 1000);
}

#[test]
fn kill_points_match_asss() {
    let mut n = 0;
    for line in KILLS
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let v: Vec<i32> = line
            .split_whitespace()
            .skip(1)
            .map(|s| s.parse().expect("int"))
            .collect();
        let s = KillSettings {
            fixed_reward: v[5],
            flag_min_bounty: v[6],
            per_killed_flag: v[7],
            per_carried_flag: v[8],
            per_team_flag: v[9],
            team_kill_points: v[10] != 0,
        };
        // the harness sets the killer's live bounty to the packet bounty
        let got = kill_points(v[0], v[1], v[2], v[3], v[4] != 0, v[0], &s);
        assert_eq!(got, v[11], "{line}");
        n += 1;
    }
    assert!(n > 7000);
}

#[test]
fn periodic_and_warzone_rewards_match_asss() {
    let mut n = 0;
    for line in PERIODIC
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let v: Vec<i32> = line
            .split_whitespace()
            .skip(1)
            .map(|s| s.parse().expect("int"))
            .collect();
        assert_eq!(periodic_points(v[0], v[1], v[2]), v[3], "{line}");
        n += 1;
    }
    for line in FLAGS
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let v: Vec<i32> = line
            .split_whitespace()
            .skip(1)
            .map(|s| s.parse().expect("int"))
            .collect();
        assert_eq!(warzone_reward(v[0], v[2]), v[3], "{line}");
        n += 1;
    }
    assert!(n > 150);
}
