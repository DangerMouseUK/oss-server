//! The SubSpace transport: [`oss_net::Server`] behind [`Transport`].
//!
//! Connection requests are offered to the registered [`Acceptor`]s in order:
//! first those modules registered with [`register_acceptor`] (a private build
//! adds the Continuum handshake this way), then OpenSec, then VIE, and last --
//! only when `Security:AllowUnencrypted` is set -- no encryption at all.
//!
//! **Everything an unset zone can land on is encrypted.** VIE is the floor, and
//! the unencrypted acceptor is not a rung below it: it is off unless an operator
//! turned it on, it is asked last so it sees only what VIE declined, and a client
//! that did not itself ask for an unencrypted session refuses its answer. A
//! connection runs in the clear only when both ends have been set to allow it.
//!
//! A connection becomes a player once its handshake completes; its game packets go to
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
///
/// The contract for anything registered here: a cipher that cannot answer an
/// in-session key exchange must refuse rather than return a key it guessed.
/// Ignoring the packet, which is what [`oss_net::Cipher::handle_core`] does by
/// default, is that refusal. A guessed key fails silently, with both ends
/// talking on and neither able to tell the traffic from corruption, so a
/// refusal an operator can see is always the better failure.
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
        // Read before the acceptors are built, because registering it is the
        // last thing done below and `zone` is borrowed mutably in between.
        let allow_unencrypted = g.get_int("Security", "AllowUnencrypted", 0) != 0;
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
        // No encryption at all: off unless an operator turned it on, and last in
        // the order so it is offered only the requests every encrypting acceptor
        // declined -- which, after VIE, is a client key of 0, the protocol's way
        // of asking for no encryption. Nothing negotiates its way here: a client
        // that did not ask for an unencrypted session refuses this answer, so
        // both ends have to have been set. See `oss-core`
        // docs/protocol/encryption.md.
        if allow_unencrypted {
            tracing::warn!(
                target: "net",
                "Security:AllowUnencrypted is set: this zone accepts connections with no \
                 encryption from clients that ask for one. For development only -- a login \
                 on such a connection carries the player's password in the clear"
            );
            server.add_acceptor(Box::new(oss_net::NullAcceptor));
        }
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

    /// Offers one raw datagram to the registered acceptors and reports the
    /// cipher the connection ended up with, by name. Tests use it to ask what
    /// this zone's acceptor chain actually does, rather than a copy of it.
    #[cfg(test)]
    fn offer(&mut self, data: &[u8]) -> Option<&'static str> {
        let addr: Endpoint = (0, ([127, 0, 0, 1], 5000).into());
        self.server.handle_datagram(0, addr, data);
        self.server.connection(&addr).and_then(|c| c.cipher_name())
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
                        // An OpenSec identity is proven only once a record from
                        // the client authenticates (peer_key() returns nothing
                        // before that), so pick it up with the login, which is
                        // such a record. Only proven keys are ever stored.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigManager, MemSource};
    use crate::module::ModuleCatalog;

    fn zone(global: &'static str) -> Zone {
        let cfg = ConfigManager::new(MemSource::new([("conf/global.conf", global)])).expect("cfg");
        Zone::new(cfg, ModuleCatalog::default())
    }

    /// A `00 01` connection request carrying `key`, on the SubSpace protocol.
    fn request(key: u32) -> Vec<u8> {
        let mut v = vec![0u8, 1];
        v.extend_from_slice(&key.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v
    }

    /// An unset zone encrypts everything it accepts, and answers a request for
    /// no encryption with nothing at all. Until 2026-11-16 the second line of
    /// this test would have read `Some("none")`: any peer could ask a public
    /// zone for an unencrypted session and be given one.
    #[test]
    fn an_unset_zone_will_not_run_a_connection_in_the_clear() {
        let mut z = zone("");
        let mut t = SubspaceTransport::new(&mut z);
        assert_eq!(t.offer(&request(0x8001_0000)), Some("vie"));

        let mut z = zone("");
        let mut t = SubspaceTransport::new(&mut z);
        assert_eq!(
            t.offer(&request(0)),
            None,
            "a request for no encryption must make no connection"
        );
        assert_eq!(t.connections(), 0);
    }

    /// With the toggle set, a client that asks for no encryption gets it, and a
    /// client that did not ask still gets VIE: the unencrypted acceptor is last
    /// in the order, so it only ever sees what VIE declined.
    #[test]
    fn the_toggle_serves_only_the_clients_that_ask() {
        let conf = "[ Security ]\nAllowUnencrypted = 1\n";

        let mut z = zone(conf);
        let mut t = SubspaceTransport::new(&mut z);
        assert_eq!(t.offer(&request(0)), Some("none"));

        let mut z = zone(conf);
        let mut t = SubspaceTransport::new(&mut z);
        assert_eq!(t.offer(&request(0x8001_0000)), Some("vie"));
    }
}
