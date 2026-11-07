//! Loads a real ASSS 1.4.4 zone directory unchanged.
//!
//! Set `OSS_ASSS_ZONE_DIR` to the root of an ASSS 1.4.4 binary distribution
//! (the directory containing `conf/`, `arenas/`, `maps/`). Without it these
//! tests are skipped; no zone files live in this repository.

use std::path::PathBuf;

use oss_server::config::{ConfigManager, ZoneDir};

fn zone_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("OSS_ASSS_ZONE_DIR")?);
    dir.join("conf/global.conf").is_file().then_some(dir)
}

#[test]
fn asss_144_zone_configs_load() {
    let Some(dir) = zone_dir() else {
        eprintln!("OSS_ASSS_ZONE_DIR not set; skipping");
        return;
    };
    let mut cfg = ConfigManager::new(ZoneDir::new(&dir)).expect("global.conf");
    assert_eq!(cfg.global().get_int("Listen", "Port", 0), 5000);

    let default = cfg.open_arena("(default)").expect("(default) arena.conf");
    assert!(default.warnings().is_empty(), "{:?}", default.warnings());
    assert_eq!(default.get("General", "Map"), Some("smallmap.lvl"));
    // from conf/svs/ship-warbird via conf/svs/svs.conf
    assert_eq!(default.get_int("Warbird", "SuperTime", 0), 6000);
    // svs sets 750; the arena overrides it afterwards
    assert_eq!(default.get_int("Bomb", "BombDamageLevel", 0), 7500);
    assert_eq!(default.get_int("Warbird", "XRadarStatus", -1), 0);
    // continued line keeps every module name
    let attach = default.get("Modules", "AttachModules").unwrap_or_default();
    assert!(
        attach.contains("fm_normal") && attach.contains("buy"),
        "{attach:?}"
    );

    let scf = cfg.open_arena("scf").expect("scf arena.conf");
    assert_eq!(scf.get("General", "Map"), Some("scf.lvl"));
    assert_eq!(scf.get_int("Warbird", "SuperTime", 0), 6000);

    // an arena with no directory of its own falls back to (default)
    let other = cfg.open_arena("nosucharena").expect("fallback");
    assert_eq!(other.get("General", "Map"), Some("smallmap.lvl"));

    let groups = cfg.open(None, "groupdef.conf").expect("groupdef.conf");
    assert!(groups.warnings().is_empty(), "{:?}", groups.warnings());
    assert!(groups.get("mod", "seemodchat").is_some());
    assert!(groups.get("default", "seemodchat").is_none());
}

#[test]
fn asss_144_modules_conf_loads() {
    let Some(dir) = zone_dir() else {
        eprintln!("OSS_ASSS_ZONE_DIR not set; skipping");
        return;
    };
    let zone = oss_server::runtime::ServerBuilder::new(&dir)
        .build_zone()
        .expect("zone builds");
    let loaded: Vec<&str> = zone.modules.loaded().collect();
    for m in [
        "core",
        "arenaman",
        "capman",
        "clientset",
        "cmdman",
        "chat",
        "game",
        "auth_file",
    ] {
        assert!(loaded.contains(&m), "{m} not loaded: {loaded:?}");
    }
}
