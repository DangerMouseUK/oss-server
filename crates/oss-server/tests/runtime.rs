//! The game loop runs closures sent from other threads, keeps time, and
//! unloads modules on shutdown. A module registered through the builder (as a
//! private build would) is loaded from the module list.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use oss_server::config::MemSource;
use oss_server::module::{Module, ModuleError};
use oss_server::net::Loopback;
use oss_server::registry::Owner;
use oss_server::runtime::{GameLoop, NoIo, ServerBuilder, channel};
use oss_server::zone::{Zone, ZoneStarted};

#[derive(Default)]
struct Counter {
    ticks: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
    unloaded: Arc<AtomicBool>,
}

impl Module for Counter {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        let t = self.ticks.clone();
        zone.set_timer(me, 1, 1, move |_| {
            t.fetch_add(1, Ordering::Relaxed);
            true
        });
        let s = self.started.clone();
        zone.on::<ZoneStarted>(me, oss_server::registry::Scope::Global, move |_, _| {
            s.store(true, Ordering::Relaxed);
        });
        Ok(())
    }

    fn unload(&self, _: &mut Zone, _: Owner) {
        self.unloaded.store(true, Ordering::Relaxed);
    }
}

#[test]
fn game_loop_runs_and_shuts_down() {
    let ticks = Arc::new(AtomicU32::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let unloaded = Arc::new(AtomicBool::new(false));
    let (t, s, u) = (ticks.clone(), started.clone(), unloaded.clone());
    let src = MemSource::new([
        ("conf/global.conf", "[Net]\nPingDataRefreshTime = 50\n"),
        (
            "conf/modules.conf",
            "; comment\nlog_console\ntest:counter\nno_such_module\n",
        ),
    ]);
    let mut builder =
        ServerBuilder::with_source(src).module("counter", &["test:counter"], move || Counter {
            ticks: t.clone(),
            started: s.clone(),
            unloaded: u.clone(),
        });
    let zone = builder.build_zone().expect("zone");
    assert_eq!(zone.modules.loaded().collect::<Vec<_>>(), vec!["counter"]);

    let (handle, rx) = channel();
    let game = GameLoop::new(zone, Box::new(Loopback::default()), Arc::new(NoIo), rx);
    let thread = std::thread::spawn(move || game.run());

    let (tx, done) = std::sync::mpsc::channel();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(handle.run(move |z| {
        let _ = tx.send(z.now());
    }));
    let now = done
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("closure ran");
    assert!(now.0 >= 5, "time advanced: {now:?}");
    handle.shutdown();
    let zone = thread.join().expect("game thread");
    assert!(started.load(Ordering::Relaxed));
    assert!(ticks.load(Ordering::Relaxed) >= 5);
    assert!(unloaded.load(Ordering::Relaxed));
    assert_eq!(zone.modules.loaded().count(), 0);
    assert!(zone.timers.is_empty());
}
