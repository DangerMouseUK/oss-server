//! The bundled default zone runs out of the box: its modules all load, and
//! what it sends matches ASSS 1.4.4 running the same sample zone (the
//! ASSS-compiled vectors in `fixtures/asss-vectors/`).

use std::path::Path;

use oss_server::modules::mapnewsdl::load_download;
use oss_server::runtime::ServerBuilder;

const PUBLIC: &str = include_str!("fixtures/asss-vectors/clientset-asss-public.txt");
const MAPS: &str = include_str!("fixtures/asss-vectors/map-checksum.txt");

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

#[test]
fn default_zone_loads_every_listed_module() {
    let zone = ServerBuilder::default_zone().build_zone().expect("zone");
    let loaded: Vec<&str> = zone.modules.loaded().collect();
    assert_eq!(
        loaded,
        vec![
            "arenaman",
            "capman",
            "mapnewsdl",
            "core",
            "clientset",
            "cmdman",
            "chat",
            "auth_file",
            "game",
            "playercmd"
        ]
    );
    assert_eq!(zone.cfg.global().get_int("Listen", "Port", 0), 5000);
}

#[test]
fn default_zone_matches_asss() {
    let mut zone = ServerBuilder::default_zone().build_zone().expect("zone");
    let scf = zone.cfg.open_arena("scf").expect("scf");
    let public = zone.cfg.open_arena("(public)").expect("public arena");
    assert!(public.warnings().is_empty(), "{:?}", public.warnings());
    let ours = oss_server::clientset::load(&public).block;
    let want = PUBLIC
        .lines()
        .find_map(|l| l.strip_prefix("packet "))
        .map(unhex)
        .expect("packet");
    assert_eq!(
        ours.as_bytes().as_slice(),
        want.as_slice(),
        "settings packet identical to ASSS"
    );

    let src = zone.cfg.source();
    for line in MAPS.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if let ["map", "asss", rel, _, size, crc] = f.as_slice() {
            let d = load_download(src, Path::new(rel), true, false).expect("map in default zone");
            assert_eq!(d.raw_len, size.parse::<usize>().expect("size"));
            assert_eq!(
                d.checksum,
                u32::from_str_radix(crc, 16).expect("crc"),
                "{rel}"
            );
            let inflated = {
                use std::io::Read;
                let mut out = Vec::new();
                flate2::read::ZlibDecoder::new(&d.packet[17..])
                    .read_to_end(&mut out)
                    .expect("zlib");
                out
            };
            assert_eq!(Some(inflated), src.read(Path::new(rel)));
        }
    }
    // the scf arena uses its own map
    assert_eq!(scf.get("General", "Map"), Some("scf.lvl"));
    let m = src.find(
        oss_server::config::search::LVL_SEARCH_PATH,
        &[('b', "scf"), ('m', "scf.lvl")],
    );
    assert_eq!(m.as_deref(), Some(Path::new("arenas/scf/scf.lvl")));
}

#[test]
fn init_zone_writes_a_runnable_copy() {
    let dir = std::env::temp_dir().join(format!("oss-init-zone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    oss_server::default_zone::write_to(&dir).expect("write");
    assert!(
        oss_server::default_zone::write_to(&dir).is_err(),
        "won't overwrite"
    );
    let zone = ServerBuilder::new(&dir)
        .build_zone()
        .expect("zone from disk");
    assert_eq!(zone.modules.loaded().count(), 10);
    let _ = std::fs::remove_dir_all(&dir);
}
