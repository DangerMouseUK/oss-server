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
}

impl ArenaMap {
    /// Builds from level file bytes (an empty map if they don't parse).
    pub fn from_bytes(data: &[u8]) -> Self {
        let grid = match Level::parse(data) {
            Ok(level) => level.server_tile_grid().0,
            Err(e) => {
                tracing::warn!(target: "mapdata", "bad level file: {e}");
                TileGrid::new()
            }
        };
        let crc = oss_proto::checksum::map_crc32(grid.cells());
        Self {
            grid: Arc::new(grid),
            crc,
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
