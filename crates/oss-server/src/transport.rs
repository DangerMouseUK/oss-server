//! The SubSpace transport: [`oss_net::Server`] behind [`Transport`].
//!
//! Connection requests are offered to the registered [`Acceptor`]s in order:
//! first those modules registered with [`register_acceptor`] (a private build
//! adds the Continuum handshake this way), then VIE. A connection becomes a
//! player once its handshake completes; its game packets go to
//! [`Zone::handle_packet`], and when it closes the player is kicked through the
//! normal leave path.
//!
//! The client type follows ASSS: a `00 01` key exchange is a SubSpace/VIE
//! client, `00 11` is Continuum.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use oss_net::{Acceptor, Config, Event, Server, VieAcceptor};

use crate::modules::core as core_mod;
use crate::net::{DatagramIo, ListenSpec, Listeners, Transport};
use crate::player::{ClientKind, PlayerId};
use crate::zone::Zone;

/// Acceptors registered by modules, consulted before the built-in VIE one.
#[derive(Default)]
pub struct Acceptors(Vec<Arc<dyn Acceptor>>);

/// Registers an extra connection acceptor (call from a module's `load`).
/// Acceptors registered earlier are asked first.
pub fn register_acceptor(zone: &mut Zone, acceptor: Arc<dyn Acceptor>) {
    zone.res.get_or_default::<Acceptors>().0.push(acceptor);
}

struct Shared(Arc<dyn Acceptor>);

impl Acceptor for Shared {
    fn accept(&self, now: u64, first: &[u8], out: &mut Vec<Vec<u8>>) -> Option<oss_net::Accept> {
        self.0.accept(now, first, out)
    }
}

/// A remote address on one of the listen sockets.
type Endpoint = (usize, SocketAddr);

/// Transport over the SubSpace core protocol.
pub struct SubspaceTransport {
    server: Server<Endpoint>,
    start: Instant,
    kinds: HashMap<Endpoint, ClientKind>,
    players: HashMap<Endpoint, PlayerId>,
    addrs: HashMap<PlayerId, Endpoint>,
    listeners: Vec<ListenSpec>,
    last_lag: u64,
}

impl SubspaceTransport {
    /// A transport using the zone's registered acceptors, then VIE.
    pub fn new(zone: &mut Zone) -> Self {
        let mut cfg = Config::server();
        let g = zone.cfg.global();
        if let Ok(ms) = u64::try_from(g.get_int("Net", "DropTimeout", 3000)) {
            // ASSS: Net:DropTimeout in ticks
            cfg.timeout_ms = ms.saturating_mul(10).max(1000);
        }
        // ASSS Net:MaxOutlistSize: a connection whose outgoing queue grows past
        // this is dropped (oss-net's max_queue does exactly that)
        if let Ok(n) = usize::try_from(g.get_int("Net", "MaxOutlistSize", 500)) {
            cfg.max_queue = n.max(16);
        }
        let mut server = Server::new(cfg);
        if let Ok(n) = usize::try_from(g.get_int("Net", "MaxPlayers", 1024)) {
            server.max_connections = n.clamp(1, crate::player::MAX_PLAYERS);
        }
        for a in zone
            .res
            .get::<Acceptors>()
            .map(|a| a.0.clone())
            .unwrap_or_default()
        {
            server.add_acceptor(Box::new(Shared(a)));
        }
        // OpenSec hellos are 42 bytes, VIE requests 8: each acceptor only
        // answers its own
        if let Some(a) = crate::opensec::acceptor(zone) {
            server.add_acceptor(Box::new(a));
        }
        server.add_acceptor(Box::new(VieAcceptor {
            accept_continuum: false,
        }));
        Self {
            server,
            start: Instant::now(),
            kinds: HashMap::new(),
            players: HashMap::new(),
            addrs: HashMap::new(),
            listeners: Listeners::of(zone),
            last_lag: 0,
        }
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Number of live connections (including ones still in their handshake).
    pub fn connections(&self) -> usize {
        self.server.len()
    }

    fn handle_events(&mut self, zone: &mut Zone) {
        while let Some((addr, ev)) = self.server.poll_event() {
            match ev {
                Event::Connected => {
                    let kind = self.kinds.remove(&addr).unwrap_or(ClientKind::Vie);
                    match core_mod::new_connection(zone, kind, Some(addr.1)) {
                        Some(pid) => {
                            let connect_as = self
                                .listeners
                                .get(addr.0)
                                .and_then(|l| l.connect_as.clone());
                            if let Some(p) = zone.players.get_mut(pid) {
                                p.connect_as = connect_as;
                            }
                            let cipher = self
                                .server
                                .connection(&addr)
                                .and_then(|c| c.cipher_name())
                                .unwrap_or("none");
                            if let Some(p) = zone.players.get_mut(pid) {
                                p.cipher = Some(cipher);
                            }
                            tracing::info!(target: "net", addr = %addr.1, listen = addr.0, pid = pid.0, ?kind, cipher, "client connected");
                            self.players.insert(addr, pid);
                            self.addrs.insert(pid, addr);
                        }
                        None => {
                            tracing::warn!(target: "net", addr = %addr.1, "player table full");
                            self.server.disconnect(&addr);
                        }
                    }
                }
                Event::Packet(p) => {
                    if let Some(&pid) = self.players.get(&addr) {
                        // an OpenSec identity is proven only once the client's
                        // first record authenticates: pick it up with the login
                        if matches!(
                            p.first(),
                            Some(&oss_proto::c2s::kind::LOGIN | &oss_proto::c2s::kind::CONT_LOGIN)
                        ) && let Some(key) =
                            self.server.connection(&addr).and_then(|c| c.peer_key())
                            && let Some(pl) = zone.players.get_mut(pid)
                        {
                            pl.identity_key = Some(key);
                        }
                        zone.handle_packet(pid, &p);
                    }
                }
                Event::Disconnected(reason) => {
                    self.kinds.remove(&addr);
                    if let Some(pid) = self.players.remove(&addr) {
                        self.addrs.remove(&pid);
                        tracing::info!(target: "net", addr = %addr.1, pid = pid.0, ?reason, "connection closed");
                        core_mod::kick(zone, pid);
                    }
                }
                _ => {}
            }
        }
    }

    fn transmit(&mut self, io: &dyn DatagramIo) {
        while let Some(((listen, addr), d)) = self.server.poll_transmit() {
            io.send_to(listen, &d, addr);
        }
    }
}

impl Transport for SubspaceTransport {
    fn on_datagram(
        &mut self,
        zone: &mut Zone,
        io: &dyn DatagramIo,
        listen: usize,
        from: SocketAddr,
        data: &[u8],
    ) {
        let from = (listen, from);
        if data.len() >= 2
            && data[0] == 0
            && (data[1] == 0x01 || data[1] == 0x11)
            && !self.players.contains_key(&from)
        {
            let kind = if data[1] == 0x11 {
                ClientKind::Continuum
            } else {
                ClientKind::Vie
            };
            // Listen:AllowVIE / AllowCont: refuse silently, as ASSS does
            let allowed = self.listeners.get(listen).is_none_or(|l| match kind {
                ClientKind::Continuum => l.allow_cont,
                _ => l.allow_vie,
            });
            if !allowed {
                tracing::debug!(target: "net", addr = %from.1, listen, ?kind, "client type not allowed on this port");
                return;
            }
            self.kinds.insert(from, kind);
        }
        let now = self.now();
        let tick = zone.now();
        if let Some(&pid) = self.players.get(&from)
            && let Some(p) = zone.players.get_mut(pid)
        {
            p.last_packet = Some(tick);
        }
        self.server.handle_datagram(now, from, data);
        self.handle_events(zone);
        self.transmit(io);
    }

    fn flush(&mut self, zone: &mut Zone, io: &dyn DatagramIo) {
        let now = self.now();
        let out = zone.take_outbox();
        let mut touched: std::collections::HashSet<Endpoint> = std::collections::HashSet::new();
        for item in out.items() {
            if let Some(addr) = self.addrs.get(&item.to)
                && self.server.send(addr, out.bytes(item), item.flags.reliable)
            {
                touched.insert(*addr);
            }
        }
        for (to, data) in out.sized() {
            if let Some(addr) = self.addrs.get(to)
                && let Some(c) = self.server.connection_mut(addr)
            {
                c.send_sized(data.to_vec());
                touched.insert(*addr);
            }
        }
        for d in out.disconnects() {
            let Some(&addr) = self.addrs.get(&d.player) else {
                continue;
            };
            if d.addr.is_some_and(|a| a != addr.1) {
                continue;
            }
            tracing::debug!(target: "net", addr = %addr.1, pid = d.player.0, reason = %d.reason, "dropping connection");
            self.addrs.remove(&d.player);
            self.players.remove(&addr);
            self.server.disconnect(&addr);
            touched.remove(&addr);
        }
        zone.return_outbox(out);
        for addr in touched {
            if let Some(c) = self.server.connection_mut(&addr) {
                c.flush(now);
                while let Some(d) = c.poll_transmit() {
                    io.send_to(addr.0, &d, addr.1);
                }
            }
        }
        self.transmit(io);
    }

    fn tick(&mut self, zone: &mut Zone, io: &dyn DatagramIo) {
        let now = self.now();
        self.server.tick(now);
        if now.saturating_sub(self.last_lag) >= 1000 {
            self.last_lag = now;
            for (addr, pid) in &self.players {
                if let Some(c) = self.server.connection(addr) {
                    let s = c.stats();
                    let rtt = i32::try_from(c.rtt_ms()).unwrap_or(i32::MAX);
                    crate::modules::lagdata::transport(
                        zone,
                        *pid,
                        rtt,
                        s.datagrams_sent,
                        s.datagrams_received,
                        c.peer_counts(),
                    );
                }
            }
        }
        self.handle_events(zone);
        self.transmit(io);
    }

    fn shutdown(&mut self, zone: &mut Zone, io: &dyn DatagramIo) {
        self.flush(zone, io);
        let addrs: Vec<Endpoint> = self.server.addresses().copied().collect();
        for a in addrs {
            self.server.disconnect(&a);
        }
        self.transmit(io);
    }
}
