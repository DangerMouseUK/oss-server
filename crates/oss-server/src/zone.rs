//! The zone: all game state plus the registries modules plug into. One `Zone`
//! lives on the game thread and every module function receives `&mut Zone`,
//! so game logic needs no locks.

use std::sync::Arc;

use crate::arena::{ArenaId, Arenas};
use crate::commands::{CommandFn, Commands, Invocation};
use crate::config::{ConfigManager, ZoneDir};
use crate::module::{ModuleCatalog, ModuleManager};
use crate::net::{Outbox, SendFlags};
use crate::player::{PlayerId, Players};
use crate::registry::{Event, Owner, Registry, Resources, Scope};
use crate::time::Ticks;
use crate::timers::{TimerId, Timers};

/// All zone state. See `docs/architecture.md`.
pub struct Zone {
    /// Config files (global and per arena).
    pub cfg: ConfigManager,
    /// The zone directory, when running from disk.
    pub dir: Option<ZoneDir>,
    /// Players.
    pub players: Players,
    /// Arenas.
    pub arenas: Arenas,
    /// Interfaces, event handlers and packet handlers.
    pub reg: Registry,
    /// Module state.
    pub res: Resources,
    /// Timers.
    pub timers: Timers,
    /// Commands.
    pub commands: Commands,
    /// Loaded modules.
    pub modules: ModuleManager,
    outbox: Outbox,
    now: Ticks,
    shutdown: bool,
}

impl Zone {
    /// A zone using `cfg` with modules from `catalog` available (none loaded).
    pub fn new(cfg: ConfigManager, catalog: ModuleCatalog) -> Self {
        Self {
            cfg,
            dir: None,
            players: Players::default(),
            arenas: Arenas::default(),
            reg: Registry::default(),
            res: Resources::default(),
            timers: Timers::default(),
            commands: Commands::default(),
            modules: ModuleManager::new(catalog),
            outbox: Outbox::default(),
            now: Ticks(0),
            shutdown: false,
        }
    }

    /// Current server time.
    pub fn now(&self) -> Ticks {
        self.now
    }

    /// Advances server time (the game loop does this every tick).
    pub fn set_now(&mut self, now: Ticks) {
        self.now = now;
    }

    // ---- interfaces -------------------------------------------------------

    /// The global implementation of interface `I`.
    pub fn iface<I: ?Sized + Send + Sync + 'static>(&self) -> Option<Arc<I>> {
        self.reg.get::<I>(None)
    }

    /// Interface `I` as seen from `arena` (arena-specific provider first).
    pub fn iface_in<I: ?Sized + Send + Sync + 'static>(&self, arena: ArenaId) -> Option<Arc<I>> {
        self.reg.get::<I>(Some(arena))
    }

    // ---- events -----------------------------------------------------------

    /// Registers an event handler.
    pub fn on<E: Event>(
        &mut self,
        owner: Owner,
        scope: Scope,
        f: impl Fn(&mut Zone, &E) + Send + Sync + 'static,
    ) {
        self.reg.on::<E>(owner, scope, Arc::new(f));
    }

    /// Raises an event: global handlers run first, then handlers registered
    /// for the event's arena, each in registration order.
    pub fn fire<E: Event>(&mut self, event: &E) {
        for list in self.reg.handlers(event).into_iter().flatten() {
            for entry in list.iter() {
                (entry.handler())(self, event);
            }
        }
    }

    // ---- packets ----------------------------------------------------------

    /// Registers a handler for game packets of type `kind`.
    pub fn on_packet(
        &mut self,
        owner: Owner,
        kind: u8,
        f: impl Fn(&mut Zone, PlayerId, &[u8]) + Send + Sync + 'static,
    ) {
        self.reg.on_packet(owner, kind, Arc::new(f));
    }

    /// Dispatches a decoded game packet from `from` to its handlers.
    pub fn handle_packet(&mut self, from: PlayerId, data: &[u8]) {
        let Some(&kind) = data.first() else { return };
        if self.players.get(from).is_none() {
            return;
        }
        let handlers = self.reg.packet_handlers(kind);
        if handlers.is_empty() {
            tracing::trace!(target: "net", player = from.0, kind, "unhandled packet");
        }
        for entry in handlers.iter() {
            (entry.handler())(self, from, data);
        }
    }

    /// Queues a game packet for a player.
    pub fn send(&mut self, to: PlayerId, data: &[u8], flags: SendFlags) {
        self.outbox.send(to, data, flags);
    }

    /// Queues a game packet for several players.
    pub fn send_many(
        &mut self,
        to: impl IntoIterator<Item = PlayerId>,
        data: &[u8],
        flags: SendFlags,
    ) {
        self.outbox.send_many(to, data, flags);
    }

    /// Queues a packet for everyone playing in `arena`, optionally except one player.
    pub fn send_to_arena(
        &mut self,
        arena: ArenaId,
        except: Option<PlayerId>,
        data: &[u8],
        flags: SendFlags,
    ) {
        let Zone {
            players, outbox, ..
        } = self;
        outbox.send_many(
            players
                .in_arena(arena)
                .map(|p| p.id)
                .filter(|id| Some(*id) != except),
            data,
            flags,
        );
    }

    /// Asks the transport to drop a player's connection.
    pub fn disconnect(&mut self, player: PlayerId, reason: impl Into<String>) {
        let addr = self.players.get(player).and_then(|p| p.addr);
        self.outbox.disconnect(player, addr, reason);
    }

    /// Takes the outbox (transports do this when flushing). Pair with
    /// [`Zone::return_outbox`] to keep its buffers.
    pub fn take_outbox(&mut self) -> Outbox {
        std::mem::take(&mut self.outbox)
    }

    /// Returns a drained outbox, keeping anything queued meanwhile.
    pub fn return_outbox(&mut self, mut out: Outbox) {
        out.clear();
        if self.outbox.is_empty() {
            self.outbox = out;
        }
    }

    // ---- timers and commands ----------------------------------------------

    /// Runs `f` after `delay` ticks, then every `interval` ticks while it
    /// returns `true` (`interval` 0 = once).
    pub fn set_timer(
        &mut self,
        owner: Owner,
        delay: u64,
        interval: u64,
        f: impl Fn(&mut Zone) -> bool + Send + Sync + 'static,
    ) -> TimerId {
        let now = self.now;
        self.timers.add(owner, now, delay, interval, Arc::new(f))
    }

    /// Registers a command.
    pub fn add_command(
        &mut self,
        owner: Owner,
        name: &str,
        arena: Option<ArenaId>,
        help: Option<&str>,
        f: impl Fn(&mut Zone, &Invocation<'_>) + Send + Sync + 'static,
    ) {
        let f: CommandFn = Arc::new(f);
        self.commands.add(owner, name, arena, help, f);
    }

    // ---- lifecycle --------------------------------------------------------

    /// Asks the game loop to shut the server down.
    pub fn request_shutdown(&mut self) {
        self.shutdown = true;
    }

    /// True once shutdown has been requested.
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown
    }
}

/// Raised once after all modules are loaded, before the first tick.
#[derive(Debug)]
pub struct ZoneStarted;
impl Event for ZoneStarted {}

/// Raised when the server begins shutting down, before modules unload.
#[derive(Debug)]
pub struct ZoneStopping;
impl Event for ZoneStopping {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MemSource;
    use crate::player::ClientKind;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn zone() -> Zone {
        let cfg = ConfigManager::new(MemSource::new([("conf/global.conf", "")])).expect("cfg");
        Zone::new(cfg, ModuleCatalog::default())
    }

    struct Ping(ArenaId);
    impl Event for Ping {
        fn arena(&self) -> Option<ArenaId> {
            Some(self.0)
        }
    }

    #[test]
    fn events_route_by_scope() {
        let mut z = zone();
        let a = z.arenas.create("a");
        let b = z.arenas.create("b");
        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        z.on::<Ping>(Owner(1), Scope::Global, move |_, _| {
            h.fetch_add(1, Ordering::Relaxed);
        });
        let h = hits.clone();
        z.on::<Ping>(Owner(2), Scope::Arena(a), move |_, _| {
            h.fetch_add(10, Ordering::Relaxed);
        });
        z.fire(&Ping(a));
        z.fire(&Ping(b));
        assert_eq!(hits.load(Ordering::Relaxed), 12);
        z.reg.remove_owner(Owner(2));
        z.fire(&Ping(a));
        assert_eq!(hits.load(Ordering::Relaxed), 13);
    }

    #[test]
    fn handlers_can_register_handlers_while_running() {
        let mut z = zone();
        let a = z.arenas.create("a");
        z.on::<Ping>(Owner(1), Scope::Global, |z, _| {
            z.on::<Ping>(Owner(1), Scope::Global, |_, _| {});
        });
        z.fire(&Ping(a));
        z.fire(&Ping(a));
    }

    trait Greeter: Send + Sync {
        fn greet(&self, z: &mut Zone) -> usize;
    }
    struct Hello;
    impl Greeter for Hello {
        fn greet(&self, z: &mut Zone) -> usize {
            z.players.len()
        }
    }
    struct Arenaish;
    impl Greeter for Arenaish {
        fn greet(&self, _: &mut Zone) -> usize {
            99
        }
    }

    #[test]
    fn interfaces_resolve_arena_first() {
        let mut z = zone();
        let a = z.arenas.create("a");
        z.reg
            .provide::<dyn Greeter>(Owner(1), Scope::Global, Arc::new(Hello));
        z.reg
            .provide::<dyn Greeter>(Owner(2), Scope::Arena(a), Arc::new(Arenaish));
        let g = z.iface::<dyn Greeter>().expect("global");
        assert_eq!(g.greet(&mut z), 0);
        let g = z.iface_in::<dyn Greeter>(a).expect("arena");
        assert_eq!(g.greet(&mut z), 99);
    }

    #[test]
    fn packets_and_timers() {
        let mut z = zone();
        let pid = z.players.alloc(ClientKind::Vie, z.now()).expect("pid");
        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        z.on_packet(Owner(1), 0x06, move |z, from, data| {
            h.fetch_add(u32::from(data[1]), Ordering::Relaxed);
            z.send(from, &[0x07], SendFlags::RELIABLE);
        });
        z.handle_packet(pid, &[0x06, 5]);
        z.handle_packet(pid, &[]);
        z.handle_packet(PlayerId(77), &[0x06, 5]);
        assert_eq!(hits.load(Ordering::Relaxed), 5);
        let out = z.take_outbox();
        assert_eq!(out.items().len(), 1);
        z.return_outbox(out);

        let h = hits.clone();
        z.set_timer(Owner(1), 2, 3, move |_| {
            h.fetch_add(100, Ordering::Relaxed) < 500
        });
        for t in 0..=20 {
            z.set_now(Ticks(t));
            crate::timers::run_due(&mut z);
        }
        // runs at ticks 2, 5, 8, 11, 14 and 17; the last run sees 505 and stops
        assert_eq!(hits.load(Ordering::Relaxed), 605);
        assert!(z.timers.is_empty());
    }
}
