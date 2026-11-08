//! `mapdata`: each arena's map as tiles (the part of ASSS `mapdata.c` the
//! server needs so far): tile lookups, the security map checksum and the
//! Continuum map CRC. The level is found and read like `mapnewsdl` does
//! (`General:Map` on the level search path) and big objects are expanded the
//! way the server-side reference does.

use std::path::Path;
use std::sync::Arc;

use oss_formats::lvl::{Level, TileGrid};

use crate::arena::ArenaId;
use crate::module::{Module, ModuleError};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::arenaman::{ArenaAction, ArenaActionKind};

/// An arena's parsed map.
#[derive(Clone)]
pub struct ArenaMap {
    /// The 1024x1024 grid with big objects expanded.
    pub grid: Arc<TileGrid>,
    /// [`oss_proto::checksum::map_crc32`] of the grid.
    pub crc: u32,
    /// Number of turf (static) flags in the map.
    pub turf_flags: usize,
}

impl ArenaMap {
    /// Builds from level file bytes (an empty map if they don't parse).
    pub fn from_bytes(data: &[u8]) -> Self {
        let (grid, turf_flags) = match Level::parse(data) {
            Ok(level) => {
                let (g, stats) = level.server_tile_grid();
                (g, stats.turf_flags)
            }
            Err(e) => {
                tracing::warn!(target: "mapdata", "bad level file: {e}");
                (TileGrid::new(), 0)
            }
        };
        let crc = oss_proto::checksum::map_crc32(grid.cells());
        Self {
            grid: Arc::new(grid),
            crc,
            turf_flags,
        }
    }

    /// The tile at `(x, y)` (0 outside the map).
    pub fn tile(&self, x: i32, y: i32) -> u8 {
        if (0..1024).contains(&x) && (0..1024).contains(&y) {
            self.grid.get(x as u32, y as u32)
        } else {
            0
        }
    }

    /// The security map checksum for `key` (ASSS `GetChecksum`).
    pub fn checksum(&self, key: u32) -> u32 {
        oss_proto::checksum::map_checksum(key, |x, y| self.tile(x, y))
    }
}

/// The parsed map of `arena`, if it has one.
pub fn arena_map(zone: &Zone, arena: ArenaId) -> Option<&ArenaMap> {
    zone.arenas.get(arena)?.ext.get::<ArenaMap>()
}

fn load(zone: &mut Zone, arena: ArenaId) {
    let data = super::mapnewsdl::map_path(zone, arena, None)
        .and_then(|p| zone.cfg.source().read(Path::new(&p)));
    let map = match data {
        Some(d) => ArenaMap::from_bytes(&d),
        // matches mapnewsdl's fallback: an empty "tinymap"
        None => ArenaMap::from_bytes(&[]),
    };
    if let Some(a) = zone.arenas.get_mut(arena) {
        a.ext.insert(map);
    }
}

/// The `mapdata` module.
pub struct MapDataModule;

impl Module for MapDataModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.on::<ArenaAction>(me, Scope::Global, |z, ev| match ev.action {
            ArenaActionKind::Create => load(z, ev.arena),
            ArenaActionKind::ConfChanged => load(z, ev.arena),
            ArenaActionKind::Destroy => {
                if let Some(a) = z.arenas.get_mut(ev.arena) {
                    a.ext.remove::<ArenaMap>();
                }
            }
            _ => {}
        });
        Ok(())
    }
}

impl ArenaMap {
    /// ASSS `FindEmptyTileNear`: walks an outward square spiral from
    /// `(x, y)` and returns the first empty tile, or `(x, y)` unchanged if
    /// none is found within ~35 tiles.
    pub fn find_empty_tile_near(&self, x: i32, y: i32) -> (i32, i32) {
        #[derive(Clone, Copy, PartialEq)]
        enum Dir {
            Up,
            Right,
            Down,
            Left,
        }
        let next = |d: Dir| match d {
            Dir::Up => Dir::Right,
            Dir::Right => Dir::Down,
            Dir::Down => Dir::Left,
            Dir::Left => Dir::Up,
        };
        // one step before the real start, as in ASSS
        let (mut dir, mut upto, mut remaining, mut cx, mut cy) = (Dir::Left, 0, 1, x + 1, y);
        loop {
            match dir {
                Dir::Down => cy += 1,
                Dir::Right => cx += 1,
                Dir::Up => cy -= 1,
                Dir::Left => cx -= 1,
            }
            remaining -= 1;
            if remaining == 0 {
                dir = next(dir);
                if dir == Dir::Up || dir == Dir::Down {
                    upto += 1;
                }
                remaining = upto;
            }
            if self.tile(cx, cy) != 0 {
                if upto < 35 {
                    continue;
                }
                return (x, y);
            }
            return (cx, cy);
        }
    }
}

/// ASSS `TILE_BRICK`.
pub const TILE_BRICK: u8 = 250;

impl ArenaMap {
    /// Sets one tile (copy-on-write if the grid is shared).
    pub fn set_tile(&mut self, x: i32, y: i32, tile: u8) {
        if (0..1024).contains(&x) && (0..1024).contains(&y) {
            Arc::make_mut(&mut self.grid).set(x as u32, y as u32, tile);
        }
    }

    /// ASSS `DoBrick`: places (or clears) a horizontal or vertical brick.
    pub fn do_brick(&mut self, drop: bool, x1: i32, y1: i32, x2: i32, y2: i32) {
        let tile = if drop { TILE_BRICK } else { 0 };
        if x1 == x2 {
            for y in y1..=y2 {
                self.set_tile(x1, y, tile);
            }
        } else if y1 == y2 {
            for x in x1..=x2 {
                self.set_tile(x, y1, tile);
            }
        }
    }
}

/// Mutable access to an arena's map.
pub fn arena_map_mut(zone: &mut Zone, arena: ArenaId) -> Option<&mut ArenaMap> {
    zone.arenas.get_mut(arena)?.ext.get_mut::<ArenaMap>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(tiles: &[(u32, u32, u32)]) -> ArenaMap {
        let mut b = Vec::new();
        for &(x, y, t) in tiles {
            b.extend_from_slice(&(x | (y << 12) | (t << 24)).to_le_bytes());
        }
        ArenaMap::from_bytes(&b)
    }

    #[test]
    fn empty_tile_search_spirals_out() {
        let m = map(&[
            (10, 10, 1),
            (9, 10, 1),
            (9, 9, 1),
            (10, 9, 1),
            (11, 9, 1),
            (11, 10, 1),
        ]);
        assert_eq!(m.find_empty_tile_near(50, 50), (50, 50));
        let (x, y) = m.find_empty_tile_near(10, 10);
        assert_eq!(m.tile(x, y), 0);
        assert!((x - 10).abs() <= 2 && (y - 10).abs() <= 2);
        let turf = map(&[(5, 5, 170), (6, 6, 170)]);
        assert_eq!(turf.turf_flags, 2);
    }
}
