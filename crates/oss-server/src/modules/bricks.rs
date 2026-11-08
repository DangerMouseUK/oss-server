//! `bricks`: dropped bricks (ASSS `bricks.c` and `mapdata.c`'s
//! `FindBrickEndpoints`).
//!
//! A brick request (C2S `0x1C`, a tile) becomes a wall segment of up to
//! `Brick:BrickSpan` tiles, laid out according to `Brick:BrickMode`
//! (`BRICK_VIE` snaps to the nearest wall, `BRICK_AHEAD`/`BRICK_LATERAL`
//! follow the ship's rotation, `BRICK_CAGE` drops four around the player).
//! Bricks are announced with S2C `0x21`, resent to players entering, expire
//! after `Brick:BrickTime` (+10 ticks for lag), and with
//! `Brick:CountBricksAsWalls` other bricks snap to them.

use std::sync::Arc;

use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::common::Brick;
use oss_proto::s2c::Bricks;

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{PlayerId, PlayerStatus};
use crate::prng::Prng;
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::arenaman::{ArenaAction, ArenaActionKind};
use super::core::{Action, PlayerAction};
use super::mapdata::{ArenaMap, arena_map, arena_map_mut};

/// How bricks are laid out (`Brick:BrickMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrickMode {
    /// Snap to the closest wall (default).
    #[default]
    Vie,
    /// A line ahead of the ship.
    Ahead,
    /// A line across the ship.
    Lateral,
    /// Four bricks forming a cage.
    Cage,
}

impl BrickMode {
    fn from_cfg(s: Option<&str>) -> Self {
        match s.map(str::trim) {
            Some(s) if s.eq_ignore_ascii_case("BRICK_AHEAD") => Self::Ahead,
            Some(s) if s.eq_ignore_ascii_case("BRICK_LATERAL") => Self::Lateral,
            Some(s) if s.eq_ignore_ascii_case("BRICK_CAGE") => Self::Cage,
            _ => Self::Vie,
        }
    }
}

#[derive(Default)]
struct ArenaBricks {
    next_id: u16,
    last_time: u32,
    live: Vec<Brick>,
    span: i32,
    mode: BrickMode,
    as_walls: bool,
    time: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Right,
    Down,
    Left,
}

const DIRS: [Dir; 4] = [Dir::Up, Dir::Right, Dir::Down, Dir::Left];

fn step(d: Dir, x: &mut i32, y: &mut i32, ox: &mut i32, oy: &mut i32) {
    match d {
        Dir::Down => {
            *oy = *y;
            *y += 1;
        }
        Dir::Right => {
            *ox = *x;
            *x += 1;
        }
        Dir::Up => {
            *oy = *y;
            *y -= 1;
        }
        Dir::Left => {
            *ox = *x;
            *x -= 1;
        }
    }
}

fn inside(x: i32, y: i32) -> bool {
    (0..1024).contains(&x) && (0..1024).contains(&y)
}

/// ASSS `FindBrickEndpoints`: the bricks to drop at tile `(dx, dy)` for a ship
/// facing `rotation` (0..40). `None` if the tile itself is solid.
pub fn find_brick_endpoints(
    map: &ArenaMap,
    mode: BrickMode,
    dx: i32,
    dy: i32,
    rotation: i32,
    length: i32,
    coin: u32,
) -> Option<Vec<(i32, i32, i32, i32)>> {
    let solid = |x: i32, y: i32| map.tile_wrapped(x, y) != 0;
    if solid(dx, dy) {
        return None;
    }
    let walk = |d: Dir, limit: &dyn Fn(i32) -> bool| {
        let (mut x, mut y, mut ox, mut oy, mut count) = (dx, dy, dx, dy, 0);
        while inside(x, y) && !solid(x, y) && limit(count) {
            step(d, &mut x, &mut y, &mut ox, &mut oy);
            count += 1;
        }
        (count, ox, oy)
    };
    let (best_dir, dest_x, dest_y) = match mode {
        BrickMode::Vie => {
            let mut best = (3000, Dir::Up, dx, dy);
            for d in DIRS {
                let (count, ox, oy) = walk(d, &|c| c < length);
                if count < best.0 {
                    best = (count, d, ox, oy);
                }
            }
            if best.0 == length {
                // no wall nearby: a line through the drop point
                if coin & 1 == 1 {
                    (Dir::Left, dx - length / 2, dy)
                } else {
                    (Dir::Up, dx, dy - length / 2)
                }
            } else {
                (best.1, best.2, best.3)
            }
        }
        BrickMode::Ahead => {
            let d = DIRS[(((rotation + 5) % 40) / 10).rem_euclid(4) as usize];
            let (_, ox, oy) = walk(d, &|c| c < length);
            (d, ox, oy)
        }
        BrickMode::Lateral => {
            let d = DIRS[(((rotation + 15) % 40) / 10).rem_euclid(4) as usize];
            let (_, ox, oy) = walk(d, &|c| c <= length / 2);
            (d, ox, oy)
        }
        BrickMode::Cage => return Some(cage(map, dx, dy, length)),
    };

    // from the closest point, extend back the other way
    let (sx, sy) = (dest_x, dest_y);
    let (mut x, mut y) = (sx, sy);
    let (mut ex, mut ey) = (sx, sy);
    match best_dir {
        Dir::Down => {
            while !solid(x, y) && (sy - y) < length && y >= 0 {
                ey = y;
                y -= 1;
            }
        }
        Dir::Right => {
            while !solid(x, y) && (sx - x) < length && x >= 0 {
                ex = x;
                x -= 1;
            }
        }
        Dir::Up => {
            while !solid(x, y) && (y - sy) < length && y < 1024 {
                ey = y;
                y += 1;
            }
        }
        Dir::Left => {
            while !solid(x, y) && (x - sx) < length && x < 1024 {
                ex = x;
                x += 1;
            }
        }
    }
    Some(vec![(sx.min(ex), sy.min(ey), sx.max(ex), sy.max(ey))])
}

fn cage(map: &ArenaMap, dx: i32, dy: i32, length: i32) -> Vec<(i32, i32, i32, i32)> {
    let solid = |x: i32, y: i32| map.tile_wrapped(x, y) != 0;
    let sx = (dx - length / 2).max(0);
    let sy = (dy - length / 2).max(0);
    let ex = (dx + (length + 1) / 2).min(1023);
    let ey = (dy + (length + 1) / 2).min(1023);
    let horiz = |y: i32| {
        let mut x = sx;
        while x <= ex && solid(x, y) {
            x += 1;
        }
        if x > ex {
            return (dx, dy, dx, dy);
        }
        let x1 = x;
        x += 1;
        while x <= ex && !solid(x, y) {
            x += 1;
        }
        (x1, y, x - 1, y)
    };
    let vert = |x: i32| {
        let mut y = sy;
        while y <= ey && solid(x, y) {
            y += 1;
        }
        if y > ey {
            return (dx, dy, dx, dy);
        }
        let y1 = y;
        y += 1;
        while y <= ey && !solid(x, y) {
            y += 1;
        }
        (x, y1, x, y - 1)
    };
    vec![horiz(sy), horiz(ey), vert(sx), vert(ex)]
}

fn data(zone: &mut Zone, a: ArenaId) -> Option<&mut ArenaBricks> {
    zone.arenas.get_mut(a)?.ext.get_mut::<ArenaBricks>()
}

fn expire(zone: &mut Zone, a: ArenaId) {
    let now = zone.now().wire();
    let Some(d) = data(zone, a) else { return };
    let (time, walls) = (d.time, d.as_walls);
    let mut gone = Vec::new();
    d.live.retain(|b| {
        let keep = (now.wrapping_sub(b.start_time.wrapping_add(time)) as i32) <= 0;
        if !keep {
            gone.push(*b);
        }
        keep
    });
    if walls && let Some(m) = arena_map_mut(zone, a) {
        for b in gone {
            m.do_brick(false, b.x1.into(), b.y1.into(), b.x2.into(), b.y2.into());
        }
    }
}

/// ASSS `DropBrick`: drops a brick for `freq` from `(x1, y1)` to `(x2, y2)`.
pub fn drop_brick(zone: &mut Zone, a: ArenaId, freq: i16, (x1, y1, x2, y2): (i32, i32, i32, i32)) {
    let now = zone.now().wire();
    let Some(d) = data(zone, a) else { return };
    // keep start times strictly increasing (clients key on them)
    let start = if (now.wrapping_sub(d.last_time) as i32) <= 0 {
        d.last_time.wrapping_add(1)
    } else {
        now
    };
    d.last_time = start;
    let brick = Brick {
        x1: x1 as i16,
        y1: y1 as i16,
        x2: x2 as i16,
        y2: y2 as i16,
        freq,
        brick_id: d.next_id,
        start_time: start,
    };
    d.next_id = d.next_id.wrapping_add(1);
    d.live.push(brick);
    let walls = d.as_walls;
    let mut buf = Vec::with_capacity(17);
    Bricks {
        bricks: vec![brick],
    }
    .encode(&mut buf);
    let mut flags = SendFlags::RELIABLE;
    flags.urgent = true;
    zone.send_to_arena(a, None, &buf, flags);
    tracing::debug!(target: "bricks", arena = a.0, x1, y1, x2, y2, freq, id = brick.brick_id, "brick dropped");
    if walls && let Some(m) = arena_map_mut(zone, a) {
        m.do_brick(true, x1, y1, x2, y2);
    }
}

fn on_brick(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 5 {
        tracing::warn!(target: "bricks", pid = pid.0, len = pkt.len(), "bad brick packet length");
        return;
    }
    let Ok(req) = c2s::DropBrick::decode(pkt) else {
        return;
    };
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let (Some(a), PlayerStatus::Playing) = (p.arena, p.status) else {
        return;
    };
    if p.ship.is_spec() {
        return;
    }
    let (freq, rotation) = (p.freq as i16, i32::from(p.pos.rotation));
    expire(zone, a);
    let Some((mode, span)) = data(zone, a).map(|d| (d.mode, d.span)) else {
        return;
    };
    let Some(map) = arena_map(zone, a).cloned() else {
        return;
    };
    let coin = zone.res.get_or_default::<Prng>().next_u32();
    let segs = find_brick_endpoints(&map, mode, req.x.into(), req.y.into(), rotation, span, coin);
    for seg in segs.unwrap_or_default() {
        drop_brick(zone, a, freq, seg);
    }
}

/// Brick dropping for other modules (ASSS `Ibricks`).
pub trait BrickDrop: Send + Sync {
    /// Drops a brick.
    fn drop_brick(&self, zone: &mut Zone, arena: ArenaId, freq: i16, seg: (i32, i32, i32, i32));
}

/// The `bricks` module.
pub struct BricksModule;

struct BricksImpl;

impl BrickDrop for BricksImpl {
    fn drop_brick(&self, zone: &mut Zone, arena: ArenaId, freq: i16, seg: (i32, i32, i32, i32)) {
        expire(zone, arena);
        drop_brick(zone, arena, freq, seg);
    }
}

impl Module for BricksModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn BrickDrop>(me, Scope::Global, Arc::new(BricksImpl));
        zone.on_packet(me, c2s::kind::DROP_BRICK, on_brick);
        zone.on::<ArenaAction>(me, Scope::Global, |z, ev| match ev.action {
            ArenaActionKind::Create | ArenaActionKind::ConfChanged => {
                let now = z.now().wire();
                let Some(a) = z.arenas.get_mut(ev.arena) else {
                    return;
                };
                let cfg = a.cfg.clone();
                let d = a.ext.get_or_default::<ArenaBricks>();
                if ev.action == ArenaActionKind::Create {
                    d.last_time = now;
                }
                d.as_walls = cfg.get_int("Brick", "CountBricksAsWalls", 1) != 0;
                d.span = cfg.get_int("Brick", "BrickSpan", 10);
                d.mode = BrickMode::from_cfg(cfg.get("Brick", "BrickMode"));
                d.time =
                    u32::try_from(cfg.get_int("Brick", "BrickTime", 6000)).unwrap_or(6000) + 10;
            }
            ArenaActionKind::Destroy => {
                if let Some(a) = z.arenas.get_mut(ev.arena) {
                    a.ext.remove::<ArenaBricks>();
                }
            }
            _ => {}
        });
        zone.on::<PlayerAction>(me, Scope::Global, |z, ev| {
            let (Action::EnterArena, Some(a)) = (ev.action, ev.arena) else {
                return;
            };
            expire(z, a);
            let live = data(z, a).map(|d| d.live.clone()).unwrap_or_default();
            let mut buf = Vec::with_capacity(17);
            for b in live {
                buf.clear();
                Bricks { bricks: vec![b] }.encode(&mut buf);
                z.send(ev.player, &buf, SendFlags::RELIABLE);
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(tiles: &[(u32, u32)]) -> ArenaMap {
        let mut b = Vec::new();
        for &(x, y) in tiles {
            b.extend_from_slice(&(x | (y << 12) | (1 << 24)).to_le_bytes());
        }
        ArenaMap::from_bytes(&b)
    }

    #[test]
    fn vie_bricks_snap_to_the_nearest_wall() {
        // a wall column at x = 100; dropping at (97, 50) builds leftwards
        // from the tile next to the wall
        let wall: Vec<(u32, u32)> = (0..200).map(|y| (100, y)).collect();
        let m = map(&wall);
        let segs = find_brick_endpoints(&m, BrickMode::Vie, 97, 50, 0, 10, 1).expect("drops");
        assert_eq!(segs, vec![(90, 50, 99, 50)]);
        // solid drop point: nothing
        assert!(find_brick_endpoints(&m, BrickMode::Vie, 100, 50, 0, 10, 1).is_none());
    }

    #[test]
    fn ahead_and_cage() {
        let m = map(&[]);
        // rotation 0 = up: a line from the drop point upwards
        let s = find_brick_endpoints(&m, BrickMode::Ahead, 500, 500, 0, 5, 1).expect("drops");
        assert_eq!(s.len(), 1);
        let (x1, _, x2, _) = s[0];
        assert_eq!(x1, x2);
        let c = find_brick_endpoints(&m, BrickMode::Cage, 500, 500, 0, 4, 1).expect("drops");
        assert_eq!(
            c,
            vec![
                (498, 498, 502, 498),
                (498, 502, 502, 502),
                (498, 498, 498, 502),
                (502, 498, 502, 502)
            ]
        );
    }
}
