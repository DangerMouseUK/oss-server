//! `mapnewsdl`: map, LVZ and news downloads (ASSS `mapnewsdl.c` plus the file
//! lookup parts of `mapdata.c`).
//!
//! When an arena is created its map (`General:Map`, found on the ASSS level
//! search path) is read, its CRC-32 taken and the file zlib-compressed into
//! an S2C `0x2A` packet; LVZ files from `General:LevelFiles` (a `+` prefix
//! marks optional ones) are wrapped unchanged. Clients learn the names and
//! checksums from S2C `0x29` on entry and download with C2S `0x0C` as a sized
//! transfer. If the map can't be read, ASSS's built-in "tinymap.lvl" is used.
//!
//! The news file (`General:NewsFile`, default `news.txt`) is served the same
//! way for C2S `0x0D`; its CRC-32 goes in the login response.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use oss_proto::Packet;
use oss_proto::c2s;
use oss_proto::s2c::{MapFile, MapFilename as MapFilenamePacket};
use oss_proto::wire::FixedStr;

use crate::arena::ArenaId;
use crate::config::search::{LVL_SEARCH_PATH, LVZ_SEARCH_PATH, find_file_on_path};
use crate::config::to_latin1;
use crate::module::{Module, ModuleError};
use crate::net::SendFlags;
use crate::player::{ClientKind, PlayerId, Ship};
use crate::registry::{Owner, Scope};
use crate::zone::Zone;

use super::arenaman::{ArenaAction, ArenaActionKind, MapFilename, hold, spec_freq, unhold};
use super::core::NewsChecksum;

/// Most LVZ files per arena (ASSS `MAX_LVZ_FILES`).
pub const MAX_LVZ_FILES: usize = 16;

/// ASSS's fallback map packet: "tinymap.lvl", one empty tile list.
const EMERGENCY_MAP: [u8; 29] = [
    0x2a, 0x74, 0x69, 0x6e, 0x79, 0x6d, 0x61, 0x70, 0x2e, 0x6c, 0x76, 0x6c, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x78, 0x9c, 0x63, 0x60, 0x60, 0x60, 0x04, 0x00, 0x00, 0x05, 0x00, 0x02,
];
const EMERGENCY_CHECKSUM: u32 = 0x5643_ef8a;

/// One downloadable file.
#[derive(Debug, Clone)]
pub struct Download {
    /// File name as sent (at most 16 bytes).
    pub name: String,
    /// CRC-32 of the file as stored on disk.
    pub checksum: u32,
    /// Size of the uncompressed file.
    pub raw_len: usize,
    /// The whole S2C `0x2A` packet.
    pub packet: Arc<[u8]>,
    /// Only sent to clients that ask for optional graphics.
    pub optional: bool,
}

/// The map (first) and LVZ files of an arena.
#[derive(Debug, Clone, Default)]
pub struct ArenaDownloads(pub Vec<Download>);

struct News {
    path: PathBuf,
    modified: Option<std::time::SystemTime>,
    packet: Option<Arc<[u8]>>,
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(
        Vec::with_capacity(data.len() / 2 + 64),
        Compression::default(),
    );
    // writing to a Vec can't fail
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

fn packet(kind: u8, name: &str, body: &[u8]) -> Arc<[u8]> {
    let mut p = Vec::with_capacity(17 + body.len());
    p.push(kind);
    let mut n = to_latin1(name);
    n.resize(16, 0);
    p.extend_from_slice(&n);
    p.extend_from_slice(body);
    p.into()
}

/// Loads a map (`compress`) or LVZ file into a download.
pub fn load_download(path: &std::path::Path, compress: bool, optional: bool) -> Option<Download> {
    let data = std::fs::read(path).ok()?;
    let name: String = path
        .file_name()?
        .to_string_lossy()
        .chars()
        .take(19)
        .collect();
    let checksum = oss_proto::checksum::crc32(&data);
    let body = if compress { zlib(&data) } else { data.clone() };
    let packet = packet(oss_proto::s2c::kind::MAP_DATA, &name, &body);
    if packet.len() > 256 * 1024 {
        tracing::warn!(target: "mapnewsdl", file = %path.display(), "compressed map/lvz is bigger than 256k");
    }
    Some(Download {
        name,
        checksum,
        raw_len: data.len(),
        packet,
        optional,
    })
}

/// ASSS `GetMapFilename`: resolves `name` (or the arena's `General:Map`) on
/// the level search path (`.lvl`) or the LVZ search path (anything else).
pub fn map_path(zone: &Zone, arena: ArenaId, name: Option<&str>) -> Option<PathBuf> {
    let dir = zone.dir.as_ref()?;
    let a = zone.arenas.get(arena)?;
    let name = name
        .map(str::to_owned)
        .or_else(|| a.cfg.get("General", "Map").map(str::to_owned));
    let is_lvl = name.as_deref().is_some_and(|n| n.ends_with(".lvl"));
    let mut repls = vec![('b', a.basename.as_str())];
    if let Some(n) = name.as_deref() {
        if !crate::config::search::is_valid_path(n) {
            return None;
        }
        repls.push(('m', n));
    }
    let path = if is_lvl {
        LVL_SEARCH_PATH
    } else {
        LVZ_SEARCH_PATH
    };
    find_file_on_path(dir.root(), path, &repls)
}

fn load_arena(zone: &mut Zone, arena: ArenaId) {
    let mut files = Vec::new();
    match map_path(zone, arena, None).and_then(|p| load_download(&p, true, false)) {
        Some(d) => files.push(d),
        None => {
            tracing::warn!(target: "mapnewsdl", arena = arena.0, "can't load level file, falling back to tinymap.lvl");
            files.push(Download {
                name: "tinymap.lvl".into(),
                checksum: EMERGENCY_CHECKSUM,
                raw_len: 4,
                packet: Arc::from(&EMERGENCY_MAP[..]),
                optional: false,
            });
        }
    }
    let lvzs = zone
        .arenas
        .get(arena)
        .and_then(|a| {
            a.cfg
                .get("General", "LevelFiles")
                .or_else(|| a.cfg.get("Misc", "LevelFiles"))
                .map(str::to_owned)
        })
        .unwrap_or_default();
    for item in lvzs
        .split([',', ':', ' '])
        .filter(|s| !s.is_empty())
        .take(MAX_LVZ_FILES)
    {
        let (optional, name) = match item.strip_prefix('+') {
            Some(n) => (true, n),
            None => (false, item),
        };
        match map_path(zone, arena, Some(name)).and_then(|p| load_download(&p, false, optional)) {
            Some(d) => files.push(d),
            None => {
                tracing::warn!(target: "mapnewsdl", arena = arena.0, file = name, "can't load LVZ file")
            }
        }
    }
    if let Some(a) = zone.arenas.get_mut(arena) {
        a.ext.insert(ArenaDownloads(files));
    }
}

fn downloads(zone: &Zone, arena: ArenaId) -> Option<&ArenaDownloads> {
    zone.arenas.get(arena)?.ext.get::<ArenaDownloads>()
}

/// ASSS `get_map`: the `n`th file among those this client may see.
fn nth(d: &ArenaDownloads, n: usize, want_optional: bool) -> Option<&Download> {
    d.0.iter().filter(|f| !f.optional || want_optional).nth(n)
}

/// The map module's [`MapFilename`] implementation.
pub struct MapNews;

impl MapFilename for MapNews {
    fn send_map_filename(&self, zone: &mut Zone, player: PlayerId) {
        let Some(p) = zone.players.get(player) else {
            return;
        };
        let Some(arena) = p.arena else { return };
        let (cont, want_all) = (p.kind == ClientKind::Continuum, p.want_all_lvz);
        let Some(d) = downloads(zone, arena) else {
            tracing::warn!(target: "mapnewsdl", arena = arena.0, "missing map data");
            return;
        };
        let pkt = if cont || want_all {
            MapFilenamePacket::Files(
                d.0.iter()
                    .filter(|f| !f.optional || want_all)
                    .map(|f| MapFile {
                        name: FixedStr::new(&to_latin1(&f.name)),
                        checksum: f.checksum,
                        size: f.packet.len() as u32,
                    })
                    .collect(),
            )
        } else {
            let Some(m) = d.0.first() else { return };
            MapFilenamePacket::Single {
                name: FixedStr::new(&to_latin1(&m.name)),
                checksum: m.checksum,
            }
        };
        let mut buf = Vec::with_capacity(64);
        pkt.encode(&mut buf);
        zone.send(player, &buf, SendFlags::RELIABLE);
    }
}

fn on_map_request(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 1 && pkt.len() != 3 {
        tracing::warn!(target: "mapnewsdl", pid = pid.0, len = pkt.len(), "bad map/LVZ request length");
        return;
    }
    let Some(p) = zone.players.get(pid) else {
        return;
    };
    let Some(arena) = p.arena else {
        tracing::warn!(target: "mapnewsdl", pid = pid.0, "map request before entering arena");
        return;
    };
    let (want, standard, ship, freq) = (
        p.want_all_lvz,
        matches!(p.kind, ClientKind::Vie | ClientKind::Continuum),
        p.ship,
        p.freq,
    );
    let n = if pkt.len() == 3 {
        usize::from(u16::from_le_bytes([pkt[1], pkt[2]]))
    } else {
        0
    };
    let Some(data) = downloads(zone, arena)
        .and_then(|d| nth(d, n, want))
        .map(|d| d.packet.clone())
    else {
        tracing::warn!(target: "mapnewsdl", pid = pid.0, n, "can't find lvl/lvz");
        return;
    };
    tracing::debug!(target: "mapnewsdl", pid = pid.0, n, bytes = data.len(), "sending map/lvz");
    zone.send_sized(pid, data);
    // too late to place the player directly: put them in spec the normal way
    let spec = zone.arenas.get(arena).map_or(8025, spec_freq);
    if standard && (ship != Ship::SPEC || freq != spec) {
        super::game::set_ship_and_freq(zone, pid, Ship::SPEC, spec);
    }
}

fn refresh_news(zone: &mut Zone) {
    let Some(news) = zone.res.get_mut::<News>() else {
        return;
    };
    let modified = std::fs::metadata(&news.path)
        .and_then(|m| m.modified())
        .ok();
    if modified.is_some() && modified == news.modified && news.packet.is_some() {
        return;
    }
    let Ok(raw) = std::fs::read(&news.path) else {
        if news.packet.take().is_some() || news.modified.is_none() {
            tracing::warn!(target: "mapnewsdl", file = %news.path.display(), "news file not found");
        }
        news.modified = Some(std::time::UNIX_EPOCH);
        zone.res.insert(NewsChecksum(0));
        return;
    };
    news.modified = modified;
    news.packet = Some(packet(oss_proto::s2c::kind::INCOMING_FILE, "", &zlib(&raw)));
    zone.res
        .insert(NewsChecksum(oss_proto::checksum::crc32(&raw)));
    tracing::debug!(target: "mapnewsdl", "news file reread");
}

fn on_news_request(zone: &mut Zone, pid: PlayerId, pkt: &[u8]) {
    if pkt.len() != 1 {
        tracing::warn!(target: "mapnewsdl", pid = pid.0, len = pkt.len(), "bad news request length");
        return;
    }
    match zone.res.get::<News>().and_then(|n| n.packet.clone()) {
        Some(p) => zone.send_sized(pid, p),
        None => tracing::warn!(target: "mapnewsdl", "news request, but there is no news file"),
    }
}

/// The `mapnewsdl` module.
pub struct MapNewsDlModule;

impl Module for MapNewsDlModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        zone.reg
            .provide::<dyn MapFilename>(me, Scope::Global, Arc::new(MapNews));
        zone.on_packet(me, c2s::kind::MAP_REQUEST, on_map_request);
        zone.on_packet(me, c2s::kind::NEWS_REQUEST, on_news_request);
        zone.on::<ArenaAction>(me, Scope::Global, |z, ev| match ev.action {
            ArenaActionKind::Create => {
                hold(z, ev.arena);
                load_arena(z, ev.arena);
                unhold(z, ev.arena);
            }
            ArenaActionKind::Destroy => {
                if let Some(a) = z.arenas.get_mut(ev.arena) {
                    a.ext.remove::<ArenaDownloads>();
                }
            }
            _ => {}
        });
        if let Some(dir) = zone.dir.as_ref() {
            let name = zone
                .cfg
                .global()
                .get("General", "NewsFile")
                .unwrap_or("news.txt")
                .to_owned();
            zone.res.insert(News {
                path: dir.root().join(name),
                modified: None,
                packet: None,
            });
            refresh_news(zone);
            zone.set_timer(me, 3000, 3000, |z| {
                refresh_news(z);
                true
            });
        }
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        zone.res.remove::<News>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emergency_map_inflates_to_an_empty_level() {
        use std::io::Read;
        let mut out = Vec::new();
        let mut d = flate2::read::ZlibDecoder::new(&EMERGENCY_MAP[17..]);
        d.read_to_end(&mut out).expect("inflate");
        assert_eq!(out.len(), 4);
        assert_eq!(oss_proto::checksum::crc32(&out), EMERGENCY_CHECKSUM);
    }

    #[test]
    fn optional_files_are_skipped_unless_wanted() {
        let f = |name: &str, optional| Download {
            name: name.into(),
            checksum: 0,
            raw_len: 0,
            packet: Arc::from(&[0u8][..]),
            optional,
        };
        let d = ArenaDownloads(vec![f("map", false), f("opt", true), f("req", false)]);
        assert_eq!(nth(&d, 1, false).map(|f| f.name.as_str()), Some("req"));
        assert_eq!(nth(&d, 1, true).map(|f| f.name.as_str()), Some("opt"));
        assert!(nth(&d, 3, true).is_none());
    }
}
