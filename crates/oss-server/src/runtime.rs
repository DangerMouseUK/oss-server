//! Process wiring: zone construction, the game thread and the tokio I/O tasks.
//!
//! ```text
//!   UDP game socket ──recv task──┐                 ┌── DatagramIo::send_to (non-blocking)
//!                                ├─ Inbound queue ─┤
//!   ZoneHandle::run(closure) ────┘   (bounded)     └── game thread: Transport + Zone + timers
//!   UDP ping socket ── ping task (answers from a PingSnapshot the game loop refreshes)
//! ```
//!
//! The game thread owns the [`Zone`]. It wakes for input or for the next
//! 10 ms tick, whichever comes first, so packets are handled as soon as they
//! arrive while timers keep SubSpace's 100 Hz timebase.

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{ConfSource, ConfigManager, ZoneDir};
use crate::module::{self, ModuleCatalog, ModuleError};
use crate::net::{DatagramIo, Transport};
use crate::ping::PingSnapshot;
use crate::time::Ticks;
use crate::zone::{Zone, ZoneStarted, ZoneStopping};

/// Capacity of the inbound queue; datagrams beyond it are dropped like UDP loss.
pub const INBOUND_CAPACITY: usize = 16 * 1024;
/// Most inbound items handled before the loop checks the clock again.
const BATCH: usize = 512;

/// Something for the game thread to do.
pub enum Inbound {
    /// A datagram arrived on the game socket.
    Datagram {
        /// Sender.
        from: SocketAddr,
        /// Payload.
        data: Vec<u8>,
    },
    /// Run a closure on the game thread (ASSS `RunInMain`): how async tasks
    /// (billing, database, admin API) hand results to game logic.
    Run(Box<dyn FnOnce(&mut Zone) + Send>),
    /// Stop the server.
    Shutdown,
}

/// A cloneable handle for talking to the game thread from other threads.
#[derive(Clone)]
pub struct ZoneHandle {
    tx: SyncSender<Inbound>,
}

impl ZoneHandle {
    /// Runs `f` on the game thread. Returns false if the server has stopped
    /// or the queue is full.
    pub fn run(&self, f: impl FnOnce(&mut Zone) + Send + 'static) -> bool {
        self.tx.try_send(Inbound::Run(Box::new(f))).is_ok()
    }

    /// Asks the server to shut down.
    pub fn shutdown(&self) {
        // a full queue still gets the message eventually
        let _ = self.tx.send(Inbound::Shutdown);
    }

    fn datagram(&self, from: SocketAddr, data: Vec<u8>) -> bool {
        match self.tx.try_send(Inbound::Datagram { from, data }) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                tracing::debug!(target: "net", "inbound queue full, dropping datagram");
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

/// Creates the inbound queue.
pub fn channel() -> (ZoneHandle, Receiver<Inbound>) {
    let (tx, rx) = std::sync::mpsc::sync_channel(INBOUND_CAPACITY);
    (ZoneHandle { tx }, rx)
}

/// [`DatagramIo`] over a non-blocking UDP socket.
pub struct UdpIo {
    socket: UdpSocket,
}

impl DatagramIo for UdpIo {
    fn send_to(&self, data: &[u8], to: SocketAddr) {
        if let Err(e) = self.socket.send_to(data, to) {
            tracing::trace!(target: "net", %to, "send failed: {e}");
        }
    }
}

/// Discards everything (for zones with no network, e.g. tests).
pub struct NoIo;

impl DatagramIo for NoIo {
    fn send_to(&self, _: &[u8], _: SocketAddr) {}
}

/// The game loop: owns the zone and the transport.
pub struct GameLoop {
    /// The zone.
    pub zone: Zone,
    transport: Box<dyn Transport>,
    io: Arc<dyn DatagramIo>,
    rx: Receiver<Inbound>,
    ping: Arc<Mutex<Arc<PingSnapshot>>>,
    start: Instant,
}

impl GameLoop {
    /// Assembles a loop. Modules should already be loaded into `zone`.
    pub fn new(
        zone: Zone,
        transport: Box<dyn Transport>,
        io: Arc<dyn DatagramIo>,
        rx: Receiver<Inbound>,
    ) -> Self {
        Self {
            zone,
            transport,
            io,
            rx,
            ping: Arc::new(Mutex::new(Arc::new(PingSnapshot::default()))),
            start: Instant::now(),
        }
    }

    /// The snapshot the ping responder serves.
    pub fn ping_snapshot(&self) -> Arc<Mutex<Arc<PingSnapshot>>> {
        self.ping.clone()
    }

    fn handle(&mut self, msg: Inbound) {
        match msg {
            Inbound::Datagram { from, data } => {
                self.transport
                    .on_datagram(&mut self.zone, self.io.as_ref(), from, &data);
            }
            Inbound::Run(f) => f(&mut self.zone),
            Inbound::Shutdown => self.zone.request_shutdown(),
        }
    }

    fn refresh_ping(&self) {
        let snap = Arc::new(PingSnapshot::build(&self.zone));
        if let Ok(mut g) = self.ping.lock() {
            *g = snap;
        }
    }

    fn tick(&mut self, now: Ticks, ping_every: u64, last_ping: &mut Ticks) {
        self.zone.set_now(now);
        crate::timers::run_due(&mut self.zone);
        self.transport.tick(&mut self.zone, self.io.as_ref());
        self.transport.flush(&mut self.zone, self.io.as_ref());
        if now.since(*last_ping) >= ping_every || last_ping.0 == 0 {
            self.refresh_ping();
            *last_ping = now;
        }
    }

    /// Runs until shutdown is requested or every [`ZoneHandle`] is dropped,
    /// then unloads all modules and returns the zone.
    pub fn run(mut self) -> Zone {
        const TICK: Duration = Duration::from_millis(10);
        let ping_every = u64::try_from(self.zone.cfg.global().get_int(
            "Net",
            "PingDataRefreshTime",
            200,
        ))
        .unwrap_or(200);
        let mut last_ping = Ticks(0);
        self.zone.fire(&ZoneStarted);
        self.refresh_ping();
        let mut tick_no: u64 = 0;
        let mut next = self.start + TICK;
        while !self.zone.shutdown_requested() {
            let now = Instant::now();
            if now >= next {
                tick_no = (now - self.start).as_millis() as u64 / 10;
                self.tick(Ticks(tick_no), ping_every.max(1), &mut last_ping);
                next = self.start + TICK * (tick_no as u32 + 1);
                continue;
            }
            match self.rx.recv_timeout(next - now) {
                Ok(msg) => {
                    self.handle(msg);
                    for _ in 0..BATCH {
                        match self.rx.try_recv() {
                            Ok(msg) => self.handle(msg),
                            Err(_) => break,
                        }
                    }
                    self.transport.flush(&mut self.zone, self.io.as_ref());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        tracing::info!(target: "core", ticks = tick_no, "shutting down");
        self.zone.fire(&ZoneStopping);
        self.transport.shutdown(&mut self.zone, self.io.as_ref());
        module::unload_all(&mut self.zone);
        self.transport.flush(&mut self.zone, self.io.as_ref());
        self.zone
    }
}

/// Errors starting a server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// `global.conf` missing.
    #[error(transparent)]
    Config(#[from] crate::config::NotFound),
    /// A module failed to load.
    #[error(transparent)]
    Module(#[from] ModuleError),
    /// Socket or runtime trouble.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

type TransportFactory = Box<dyn FnOnce(&mut Zone) -> Box<dyn Transport> + Send>;

/// Builds and runs a server. Private builds use this to add modules.
///
/// ```no_run
/// # use oss_server::runtime::ServerBuilder;
/// ServerBuilder::new("zone")
///     // .module("security:mysecurity", &[], || MySecurity::default())
///     .run()
///     .expect("server");
/// ```
pub struct ServerBuilder {
    source: Option<Box<dyn ConfSource + Send + Sync>>,
    dir: Option<PathBuf>,
    catalog: ModuleCatalog,
    modules: Option<Vec<String>>,
    transport: Option<TransportFactory>,
    bind: Option<SocketAddr>,
}

impl ServerBuilder {
    /// A server for the zone directory `dir`, with the built-in modules
    /// available.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            source: None,
            dir: Some(dir.into()),
            catalog: crate::modules::builtin_catalog(),
            modules: None,
            transport: None,
            bind: None,
        }
    }

    /// A server whose config comes from `source` instead of a directory
    /// (tests, embedded zones).
    pub fn with_source(source: impl ConfSource + Send + Sync + 'static) -> Self {
        Self {
            source: Some(Box::new(source)),
            dir: None,
            catalog: crate::modules::builtin_catalog(),
            modules: None,
            transport: None,
            bind: None,
        }
    }

    /// Makes a module available under `name` and `aliases` (replacing any
    /// built-in module of the same name). It is loaded if the module list
    /// (`modules.conf` or [`ServerBuilder::modules`]) names it.
    pub fn module<M: module::Module>(
        mut self,
        name: &str,
        aliases: &[&str],
        factory: impl Fn() -> M + Send + Sync + 'static,
    ) -> Self {
        self.catalog.register(name, aliases, factory);
        self
    }

    /// Loads exactly these modules, in order, instead of `conf/modules.conf`.
    pub fn modules<S: Into<String>>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.modules = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Uses a custom transport (default: the SubSpace protocol transport).
    pub fn transport(
        mut self,
        f: impl FnOnce(&mut Zone) -> Box<dyn Transport> + Send + 'static,
    ) -> Self {
        self.transport = Some(Box::new(f));
        self
    }

    /// Overrides the listen address (default: `0.0.0.0:<Listen:Port>`).
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.bind = Some(addr);
        self
    }

    /// Builds the zone and loads its modules, without any network.
    pub fn build_zone(&mut self) -> Result<Zone, ServerError> {
        let (cfg, dir, names) = match (self.source.take(), &self.dir) {
            (Some(src), _) => {
                let names = module::read_modules_conf(src.as_ref());
                (ConfigManager::new(BoxedSource(src))?, None, names)
            }
            (None, Some(d)) => {
                let zd = ZoneDir::new(d);
                let names = module::read_modules_conf(&zd);
                (ConfigManager::new(zd.clone())?, Some(zd), names)
            }
            (None, None) => {
                return Err(ServerError::Io(std::io::Error::other(
                    "server already built",
                )));
            }
        };
        let mut zone = Zone::new(cfg, std::mem::take(&mut self.catalog));
        zone.dir = dir;
        let names = self.modules.take().or(names).unwrap_or_else(|| {
            crate::modules::DEFAULT_MODULES
                .iter()
                .map(|s| (*s).to_owned())
                .collect()
        });
        for name in &names {
            match module::load(&mut zone, name) {
                Ok(_) => {}
                Err(ModuleError::Unknown(n)) => {
                    tracing::warn!(target: "modman", module = %n, "no such module, skipping");
                }
                Err(ModuleError::AlreadyLoaded(n)) => {
                    tracing::warn!(target: "modman", module = %n, "listed twice, skipping");
                }
                Err(e) => return Err(e.into()),
            }
        }
        module::post_load_all(&mut zone);
        Ok(zone)
    }

    /// Builds everything and runs until shutdown (Ctrl-C / SIGTERM). Blocks.
    pub fn run(self) -> Result<(), ServerError> {
        let server = self.start()?;
        let mut done = server.done.clone();
        server.rt.block_on(async {
            tokio::select! {
                _ = shutdown_signal() => {
                    tracing::info!(target: "core", "shutdown signal received");
                }
                _ = done.wait_for(|d| *d) => {}
            }
        });
        server.stop();
        Ok(())
    }

    /// Builds the zone, binds the sockets and starts the game thread and I/O
    /// tasks, returning immediately. Use [`RunningServer::stop`] to shut down.
    pub fn start(mut self) -> Result<RunningServer, ServerError> {
        let mut zone = self.build_zone()?;
        let port = u16::try_from(zone.cfg.global().get_int("Listen", "Port", 5000)).unwrap_or(5000);
        let bind = self
            .bind
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], port)));
        let game = UdpSocket::bind(bind)?;
        game.set_nonblocking(true)?;
        let mut ping_addr = bind;
        if bind.port() != 0 {
            ping_addr.set_port(bind.port().wrapping_add(1));
        }
        let ping = UdpSocket::bind(ping_addr)?;
        ping.set_nonblocking(true)?;
        let game_addr = game.local_addr()?;
        let ping_addr = ping.local_addr()?;
        tracing::info!(target: "net", game = %game_addr, ping = %ping_addr, "listening");

        let transport = match self.transport.take() {
            Some(f) => f(&mut zone),
            None => crate::modules::default_transport(&mut zone),
        };
        let (handle, rx) = channel();
        zone.res.insert(handle.clone());
        let io: Arc<dyn DatagramIo> = Arc::new(UdpIo {
            socket: game.try_clone()?,
        });
        let game_loop = GameLoop::new(zone, transport, io, rx);
        let snapshot = game_loop.ping_snapshot();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("oss-io")
            .enable_all()
            .build()?;
        let (done_tx, done) = tokio::sync::watch::channel(false);
        let game_thread = std::thread::Builder::new()
            .name("game".into())
            .spawn(move || {
                let zone = game_loop.run();
                let _ = done_tx.send(true);
                zone
            })?;

        let tasks = rt.block_on(async {
            let game = tokio::net::UdpSocket::from_std(game)?;
            let ping = tokio::net::UdpSocket::from_std(ping)?;
            let recv_handle = handle.clone();
            let recv = tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                loop {
                    match game.recv_from(&mut buf).await {
                        Ok((n, from)) => {
                            if !recv_handle.datagram(from, buf[..n].to_vec()) {
                                break;
                            }
                        }
                        // e.g. ICMP port unreachable reported on Windows
                        Err(e) => tracing::trace!(target: "net", "recv: {e}"),
                    }
                }
            });
            let pinger = tokio::spawn(async move {
                let mut buf = [0u8; 16];
                loop {
                    let Ok((n, from)) = ping.recv_from(&mut buf).await else {
                        continue;
                    };
                    let snap = snapshot.lock().map(|g| g.clone()).ok();
                    if let Some(reply) = snap.and_then(|s| s.reply(&buf[..n])) {
                        let _ = ping.send_to(&reply, from).await;
                    }
                }
            });
            Ok::<_, std::io::Error>(vec![recv, pinger])
        })?;
        Ok(RunningServer {
            rt,
            handle,
            game_thread,
            tasks,
            done,
            game_addr,
            ping_addr,
        })
    }
}

/// A server started with [`ServerBuilder::start`].
pub struct RunningServer {
    rt: tokio::runtime::Runtime,
    handle: ZoneHandle,
    game_thread: std::thread::JoinHandle<Zone>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    done: tokio::sync::watch::Receiver<bool>,
    game_addr: SocketAddr,
    ping_addr: SocketAddr,
}

impl RunningServer {
    /// The bound game port.
    pub fn game_addr(&self) -> SocketAddr {
        self.game_addr
    }

    /// The bound ping port.
    pub fn ping_addr(&self) -> SocketAddr {
        self.ping_addr
    }

    /// A handle for running closures on the game thread.
    pub fn handle(&self) -> ZoneHandle {
        self.handle.clone()
    }

    /// True once the game loop has stopped.
    pub fn is_stopped(&self) -> bool {
        *self.done.borrow()
    }

    /// Shuts down gracefully and returns the final zone state, if the game
    /// thread finished cleanly.
    pub fn stop(self) -> Option<Zone> {
        self.handle.shutdown();
        let zone = self.game_thread.join().ok();
        for t in &self.tasks {
            t.abort();
        }
        self.rt.shutdown_timeout(Duration::from_secs(1));
        zone
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

struct BoxedSource(Box<dyn ConfSource + Send + Sync>);

impl ConfSource for BoxedSource {
    fn open(&self, arena: Option<&str>, name: &str) -> Option<(PathBuf, Vec<u8>)> {
        self.0.open(arena, name)
    }
}
