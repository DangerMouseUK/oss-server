//! Hot reload: edited config files reach running arenas (and modules, through
//! `ConfChanged` / `GlobalConfChanged`), and modules can be loaded, unloaded,
//! attached and detached while the zone runs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oss_proto::Packet;
use oss_proto::c2s::{self, GoArena, Login};
use oss_proto::common::chat_type;
use oss_proto::s2c::{self, ChatBody, S2CChat};
use oss_proto::wire::FixedStr;

use oss_server::modules::arenaman::{ArenaAction, ArenaActionKind, GlobalConfChanged};
use oss_server::modules::core as core_mod;
use oss_server::net::Loopback;
use oss_server::player::{ClientKind, PlayerId};
use oss_server::registry::{Owner, Scope};
use oss_server::runtime::ServerBuilder;
use oss_server::time::Ticks;
use oss_server::zone::Zone;

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Makes each directory name unique within the process. The tests in this
/// binary run as threads of one process, so the pid is the same for all of
/// them, and `as_nanos` has nanosecond units but not nanosecond resolution
/// -- two tests entering here together could agree on a name. They then
/// shared a directory, and whichever finished first had its `TempDir::drop`
/// remove it out from under the other, whose next `rewrite` failed with
/// NotFound. A counter makes the uniqueness structural rather than a bet on
/// clock resolution.
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_zone(files: &[(&str, &str)]) -> TempDir {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("oss-hot-{}-{nanos}-{n}", std::process::id()));
    // create_dir, not create_dir_all: if this name is somehow taken we want
    // to hear about it here rather than quietly share someone else's files
    std::fs::create_dir(&dir).expect("a directory of this name already exists");
    // owned before anything is written, so a failure below still cleans up
    let owned = TempDir(dir);
    for (path, data) in files {
        let p = owned.0.join(path);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, data).expect("write");
    }
    owned
}

fn rewrite(dir: &TempDir, path: &str, data: &str) {
    // make sure the timestamp moves even on coarse file systems
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(dir.0.join(path), data).expect("write");
}

struct H {
    zone: Zone,
    lo: Loopback,
    t: u64,
}

impl H {
    fn tick(&mut self, n: u64) {
        for _ in 0..n {
            self.t += 1;
            self.zone.set_now(Ticks(self.t));
            oss_server::timers::run_due(&mut self.zone);
            self.lo.collect(&mut self.zone);
        }
    }

    fn player(&mut self, name: &str) -> PlayerId {
        let p = core_mod::new_connection(&mut self.zone, ClientKind::Vie, None).expect("slot");
        let mut buf = Vec::new();
        Login {
            kind: c2s::kind::LOGIN,
            flags: 0,
            name: FixedStr::new(name.as_bytes()),
            password: FixedStr::new(b"x"),
            machine_id: 1,
            connect_type: 4,
            timezone_bias: 0,
            unknown1: 0,
            client_version: 134,
            field444: 444,
            bound_ip: 0,
            permission_id: 1,
            server_ip: 0,
            bound_port: 0,
            redirect_id: 0,
            using_proxy: 0,
            unused: 0,
            cont_id: None,
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.tick(10);
        buf.clear();
        GoArena {
            ship: 8,
            x_res: 1024,
            y_res: 768,
            arena_type: -1,
            ..Default::default()
        }
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.tick(10);
        p
    }

    fn cmd(&mut self, p: PlayerId, text: &str) -> Vec<String> {
        self.lo.take(p);
        let mut buf = Vec::new();
        c2s::C2SChat(ChatBody {
            kind: chat_type::PUBLIC,
            sound: 0,
            pid: 0,
            text: text.as_bytes().to_vec(),
        })
        .encode(&mut buf);
        self.zone.handle_packet(p, &buf);
        self.lo.collect(&mut self.zone);
        self.lo
            .take(p)
            .into_iter()
            .filter(|(d, _)| d[0] == s2c::kind::CHAT)
            .map(|(d, _)| {
                String::from_utf8_lossy(&S2CChat::decode(&d).expect("chat").0.text).into_owned()
            })
            .collect()
    }
}

#[test]
fn setting_commands_change_config_live() {
    let dir = temp_zone(&[
        ("conf/global.conf", "[Misc]\nGreeting = one\n"),
        ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
        (
            "conf/groupdef.conf",
            "[default]\ncmd_setg = 1\ncmd_seta = 1\ncmd_getg = 1\ncmd_geta = 1\n",
        ),
        ("conf/staff.conf", ""),
        ("arenas/(default)/arena.conf", "[Misc]\nValue = 1\n"),
    ]);
    let zone = ServerBuilder::new(&dir.0).build_zone().expect("zone");
    let mut h = H {
        zone,
        lo: Loopback::default(),
        t: 0,
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (s1, s2) = (seen.clone(), seen.clone());
    h.zone
        .on::<ArenaAction>(Owner(999), Scope::Global, move |_, ev| {
            if ev.action == ArenaActionKind::ConfChanged {
                s1.lock().expect("lock").push("arena");
            }
        });
    h.zone
        .on::<GlobalConfChanged>(Owner(999), Scope::Global, move |_, _| {
            s2.lock().expect("lock").push("global");
        });
    let p = h.player("alice");
    let arena = h.zone.players.get(p).and_then(|x| x.arena).expect("arena");

    let out = h.cmd(p, "?getg Misc:Greeting");
    assert!(out.iter().any(|t| t == "Misc:Greeting=one"), "{out:?}");
    let out = h.cmd(p, "?getg Misc:Nothing");
    assert!(out.iter().any(|t| t == "Misc:Nothing not found"), "{out:?}");

    // a permanent change reaches the running zone and the file
    h.cmd(p, "?setg Misc:Greeting=three");
    assert_eq!(h.zone.cfg.global().get("Misc", "Greeting"), Some("three"));
    let text = std::fs::read_to_string(dir.0.join("conf/global.conf")).expect("read");
    assert!(text.contains("Misc:Greeting = three"), "{text}");
    assert!(text.contains("; set by alice at "), "{text}");

    // -t changes the running zone only
    h.cmd(p, "?setg -t Misc:Greeting=four");
    assert_eq!(h.zone.cfg.global().get("Misc", "Greeting"), Some("four"));
    let text = std::fs::read_to_string(dir.0.join("conf/global.conf")).expect("read");
    assert!(!text.contains("four"), "{text}");

    // ?seta is the same for the arena's own config
    h.cmd(p, "?seta Misc:Value=7");
    assert_eq!(
        h.zone
            .arenas
            .get(arena)
            .map(|a| a.cfg.get_int("Misc", "Value", 0)),
        Some(7)
    );
    let out = h.cmd(p, "?geta Misc:Value");
    assert!(out.iter().any(|t| t == "Misc:Value=7"), "{out:?}");

    // ASSS also takes a second colon in place of the equals sign
    h.cmd(p, "?seta Misc:Value:9");
    assert_eq!(
        h.zone
            .arenas
            .get(arena)
            .map(|a| a.cfg.get_int("Misc", "Value", 0)),
        Some(9)
    );
    // every change told the arena and the zone
    let seen = seen.lock().expect("lock").clone();
    assert_eq!(
        seen,
        vec!["global", "global", "arena", "arena"],
        "each ?set* fires the change its file belongs to"
    );
    drop(h);
}

#[test]
fn config_and_modules_reload_live() {
    let dir = temp_zone(&[
        ("conf/global.conf", "[Misc]\nGreeting = one\n"),
        ("conf/passwd.conf", "[General]\nAllowUnknown = yes\n"),
        (
            "conf/groupdef.conf",
            "[default]\ncmd_reloadconf = 1\ncmd_insmod = 1\ncmd_rmmod = 1\ncmd_attmod = 1\ncmd_lsmod = 1\n",
        ),
        ("conf/staff.conf", ""),
        ("arenas/(default)/arena.conf", "[Misc]\nValue = 1\n"),
    ]);
    let zone = ServerBuilder::new(&dir.0).build_zone().expect("zone");
    let mut h = H {
        zone,
        lo: Loopback::default(),
        t: 0,
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (s1, s2) = (seen.clone(), seen.clone());
    h.zone
        .on::<ArenaAction>(Owner(999), Scope::Global, move |_, ev| {
            if ev.action == ArenaActionKind::ConfChanged {
                s1.lock().expect("lock").push("arena");
            }
        });
    h.zone
        .on::<GlobalConfChanged>(Owner(999), Scope::Global, move |_, _| {
            s2.lock().expect("lock").push("global");
        });
    let p = h.player("alice");
    let arena = h.zone.players.get(p).and_then(|x| x.arena).expect("arena");
    let value = |h: &H| {
        h.zone
            .arenas
            .get(arena)
            .map(|a| a.cfg.get_int("Misc", "Value", 0))
    };
    assert_eq!(value(&h), Some(1));

    // ?reloadconf picks up both files at once
    rewrite(&dir, "arenas/(default)/arena.conf", "[Misc]\nValue = 2\n");
    rewrite(&dir, "conf/global.conf", "[Misc]\nGreeting = two\n");
    let out = h.cmd(p, "?reloadconf");
    assert!(
        out.iter()
            .any(|t| t == "Reloaded 2 changed config file(s)."),
        "{out:?}"
    );
    assert_eq!(value(&h), Some(2));
    assert_eq!(h.zone.cfg.global().get("Misc", "Greeting"), Some("two"));
    assert_eq!(*seen.lock().expect("lock"), vec!["arena", "global"]);

    // and the periodic check (Config:CheckModifiedFilesInterval, 15 s) does too
    rewrite(&dir, "arenas/(default)/arena.conf", "[Misc]\nValue = 3\n");
    h.tick(1600);
    assert_eq!(value(&h), Some(3));

    // modules at run time
    assert!(h.zone.modules.is_loaded("freqowners"));
    let out = h.cmd(p, "?rmmod freqowners");
    assert!(
        out.iter()
            .any(|t| t == "Module freqowners unloaded successfully")
    );
    assert!(!h.zone.modules.is_loaded("freqowners"));
    assert!(h.zone.commands.lookup("freqkick", None).is_empty());
    let out = h.cmd(p, "?insmod freqowners");
    assert!(
        out.iter()
            .any(|t| t == "Module freqowners loaded successfully")
    );
    assert!(!h.zone.commands.lookup("freqkick", None).is_empty());
    let out = h.cmd(p, "?insmod nosuchmodule");
    assert!(
        out.iter()
            .any(|t| t == "Loading module nosuchmodule failed")
    );
    let out = h.cmd(p, "?attmod koth");
    assert!(out.iter().any(|t| t == "Module koth attached."), "{out:?}");
    let out = h.cmd(p, "?attmod -d koth");
    assert!(out.iter().any(|t| t == "Module koth detached."), "{out:?}");
    drop(h);
}
