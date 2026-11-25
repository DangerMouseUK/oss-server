//! The SubSpace transport: [`oss_net::Server`] behind [`Transport`].
//!
//! **Each security mechanism this zone can speak is described once**, in an
//! [`oss_net::MechanismRegistry`], and everything else is read from that: the
//! two start-up lines and the acceptors that connection requests are offered
//! to. Built in are `none` (no encryption), `vie` (the SubSpace cipher) and
//! `opensec`. A module adds another with [`register_mechanism`]; a private
//! build may supply one that way.
//!
//! What the zone serves is an [`oss_net::MechanismPolicy`] worked out from the
//! registry and two settings. Requests are offered to its acceptors strongest
//! first: OpenSec, then any module mechanism ranked between it and VIE, then
//! VIE, and last -- only when `Security:AllowUnencrypted` is set -- no
//! encryption at all. `[OpenSec] Enabled = 0` leaves OpenSec out. The order
//! only matters where two acceptors would take the same first datagram, and
//! the built-in ones never do: each takes only its own opening.
//!
//! At start the zone logs two lines, both read from the registry:
//! `Mechanisms supported: None, Subspace, OpenSecurity` (what this program can
//! speak) and `Mechanisms enabled on this zone: …` (what the operator has
//! switched on).
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
//! client, `00 11` is Continuum. It is read from the first packet, not from
//! which mechanism accepted the connection.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use oss_net::mechanism::OpenSecSetup;
use oss_net::opensec::os_entropy;
use oss_net::{
    Config, Event, Mechanism, MechanismError, MechanismId, MechanismPolicy, MechanismRegistry,
    Server, VieAcceptor,
};

use crate::modules::core as core_mod;
use crate::net::{DatagramIo, ListenSpec, Listeners, Transport};
use crate::player::{ClientKind, PlayerId};
use crate::registry::Owner;
use crate::zone::Zone;

/// Mechanisms registered by modules, each with the module that registered it,
/// added to the built-in ones when the transport is built.
#[derive(Default)]
struct ModuleMechanisms(Vec<(Owner, Mechanism)>);

/// Registers an extra security mechanism for the module `me` (call from its
/// `load`, passing the `me` it was given).
///
/// It joins the built-in ones by its strength, wherever it was registered
/// from: it is named in the start-up lines, and if it is automatic and has a
/// server half, connection requests are offered to its acceptor in strength
/// order. A mechanism that may only be asked for by name is listed as
/// supported but not served. Only modules loaded before the server starts
/// count, because the transport reads the registrations once, when it is
/// built.
///
/// The registration belongs to `me` and is dropped when that module unloads,
/// or when its `load` fails, so the module can be loaded again. A transport
/// already built keeps what it read.
///
/// Refused, so the module's load can fail, if its identifier or display name
/// is already taken, by a built-in mechanism or one registered earlier, or if
/// it is declared automatic below the SubSpace cipher's strength.
///
/// The contract for anything registered here: a cipher that cannot answer an
/// in-session key exchange must refuse rather than return a key it guessed.
/// Ignoring the packet, which is what [`oss_net::Cipher::handle_core`] does by
/// default, is that refusal. A guessed key fails silently, with both ends
/// talking on and neither able to tell the traffic from corruption, so a
/// refusal an operator can see is always the better failure.
pub fn register_mechanism(
    zone: &mut Zone,
    me: Owner,
    mechanism: Mechanism,
) -> Result<(), MechanismError> {
    // Checked now against everything it will be registered beside, so a clash
    // fails the module that caused it rather than the start of the zone. The
    // OpenSec stand-in has no key; only its identifier and name are compared.
    let mut trial = builtin_mechanisms(Mechanism::opensec(OpenSecSetup::new(os_entropy())))?;
    let registered = zone.res.get_or_default::<ModuleMechanisms>();
    for (_, m) in &registered.0 {
        trial.register(m.clone())?;
    }
    trial.register(mechanism.clone())?;
    registered.0.push((me, mechanism));
    Ok(())
}

/// Drops the mechanisms `owner` registered (the module manager calls this when
/// a module unloads or its load fails).
pub(crate) fn forget_mechanisms(zone: &mut Zone, owner: Owner) {
    if let Some(registered) = zone.res.get_mut::<ModuleMechanisms>() {
        registered.0.retain(|(o, _)| *o != owner);
    }
}

/// Whether `Security:AllowUnencrypted` is set.
fn allow_unencrypted(zone: &Zone) -> bool {
    zone.cfg.global().get_int("Security", "AllowUnencrypted", 0) != 0
}

/// The two start-up lines, as the zone logs them: what this program can speak,
/// and what the operator has switched on.
fn startup_lines(registry: &MechanismRegistry, policy: &MechanismPolicy) -> [String; 2] {
    [registry.supported_line(), policy.enabled_line(registry)]
}

/// The mechanisms built into this server. Its SubSpace cipher does not answer
/// Continuum clients.
fn builtin_mechanisms(opensec: Mechanism) -> Result<MechanismRegistry, MechanismError> {
    let mut registry = MechanismRegistry::new();
    registry.register(Mechanism::none())?;
    registry.register(Mechanism::vie_with(VieAcceptor {
        accept_continuum: false,
    }))?;
    registry.register(opensec)?;
    Ok(registry)
}

/// This zone's registry (the built-in mechanisms and the modules' ones) and
/// its policy (what the operator has switched on). Loads the OpenSec key when
/// OpenSec is on.
fn zone_mechanisms(
    zone: &mut Zone,
) -> Result<(MechanismRegistry, MechanismPolicy), MechanismError> {
    let opensec_on = crate::opensec::enabled(zone);
    let allow_unencrypted = allow_unencrypted(zone);
    let mut registry = builtin_mechanisms(crate::opensec::mechanism(zone))?;
    let from_modules: Vec<Mechanism> = zone
        .res
        .get::<ModuleMechanisms>()
        .map(|m| m.0.iter().map(|(_, m)| m.clone()).collect())
        .unwrap_or_default();
    for m in from_modules {
        registry.register(m)?;
    }
    for m in registry
        .iter()
        .filter(|m| !m.is_automatic() && m.id().as_str() != "none")
    {
        tracing::warn!(
            target: "net",
            mechanism = %m.id(),
            "{} may only be asked for by name, so this zone does not serve it",
            m.display_name()
        );
    }
    let automatic = MechanismPolicy::automatic(&registry);
    let mut policy = if opensec_on {
        automatic
    } else {
        MechanismPolicy::from_names(
            &registry,
            automatic
                .order()
                .iter()
                .map(MechanismId::as_str)
                .filter(|id| *id != "opensec"),
        )?
    };
    if allow_unencrypted {
        policy = policy.allow_deliberately(&registry, "none")?;
    }
    Ok((registry, policy))
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
    /// A transport serving the zone's mechanisms: the built-in ones and any a
    /// module registered, as the zone's settings switch them on.
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
        match zone_mechanisms(zone) {
            Ok((registry, policy)) => {
                for line in startup_lines(&registry, &policy) {
                    tracing::info!(target: "net", "{line}");
                }
                // No encryption at all: off unless an operator turned it on,
                // and last in the order so it is offered only the requests
                // every encrypting acceptor declined -- which, after VIE, is a
                // client key of 0, the protocol's way of asking for no
                // encryption. Nothing negotiates its way here: a client that
                // did not ask for an unencrypted session refuses this answer,
                // so both ends have to have been set. See `oss-core`
                // docs/protocol/encryption.md.
                if allow_unencrypted(zone) {
                    tracing::warn!(
                        target: "net",
                        "Security:AllowUnencrypted is set: this zone accepts connections with no \
                         encryption from clients that ask for one. For development only -- a login \
                         on such a connection carries the player's password in the clear"
                    );
                }
                match policy.acceptors(&registry) {
                    Ok(acceptors) => {
                        for a in acceptors {
                            server.add_acceptor(a);
                        }
                    }
                    Err(e) => {
                        tracing::error!(target: "net", "can't build the acceptors ({e}): this zone accepts no connections");
                    }
                }
            }
            Err(e) => {
                tracing::error!(target: "net", "can't set up the security mechanisms ({e}): this zone accepts no connections");
            }
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

    // -----------------------------------------------------------------------
    // The mechanism registry: the start-up lines, the two switches, a module's
    // mechanism, and the order the acceptors are asked in.
    // -----------------------------------------------------------------------

    use oss_net::mechanism::{Era, Selection};
    use oss_net::opensec::{OpenSecAcceptor, OpenSecClientHandshake, ServerKey};
    use oss_net::{Accept, Acceptor, Cipher, Handshake, NullAcceptor};

    use crate::module::{Module, ModuleError};
    use crate::registry::Owner;
    use crate::runtime::ServerBuilder;

    /// The dummy's opening. It starts `00 01`, so the server offers it to the
    /// acceptors, and is 6 bytes, so neither the SubSpace acceptor (exactly 8)
    /// nor the OpenSec one (a full hello) takes it.
    const DUMMY_HELLO: [u8; 6] = [0, 1, b'D', b'U', b'M', b'Y'];

    /// XORs every byte. Secures nothing; it only has to be its own cipher.
    struct DummyCipher;

    impl Cipher for DummyCipher {
        fn encrypt(&mut self, datagram: &mut Vec<u8>) {
            datagram.iter_mut().for_each(|b| *b ^= 0x5A);
        }
        fn decrypt(&mut self, datagram: &mut Vec<u8>) -> bool {
            datagram.iter_mut().for_each(|b| *b ^= 0x5A);
            true
        }
        fn name(&self) -> &'static str {
            "dummy"
        }
    }

    struct DummyAcceptor;

    impl Acceptor for DummyAcceptor {
        fn accept(&self, _now: u64, first: &[u8], out: &mut Vec<Vec<u8>>) -> Option<Accept> {
            (first == DUMMY_HELLO).then(|| {
                out.push(vec![0, 2, b'Y', b'M', b'U', b'D']);
                Accept::Established(Box::new(DummyCipher))
            })
        }
    }

    /// A third party's mechanism, ranked between the SubSpace cipher and
    /// OpenSec, with only a server half.
    fn dummy() -> Mechanism {
        Mechanism::new(
            "dummy@example.org",
            "Dummy",
            Era::Modern,
            2000,
            Selection::Automatic,
        )
        .expect("a valid description")
        .server(|| Box::new(DummyAcceptor))
    }

    /// Registers the dummy the way any module would: from its `load`.
    struct DummyModule;

    impl Module for DummyModule {
        fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
            register_mechanism(zone, me, dummy()).map_err(|e| ModuleError::Failed {
                module: "dummy".into(),
                reason: e.to_string(),
            })
        }
    }

    /// A zone built as the server builds one, with the dummy module loaded.
    fn zone_with_dummy(global: &'static str) -> Zone {
        ServerBuilder::with_source(MemSource::new([("conf/global.conf", global)]))
            .module("dummy", &[], || DummyModule)
            .modules(["dummy"])
            .build_zone()
            .expect("zone")
    }

    /// A real OpenSec client's first datagram.
    fn opensec_hello() -> Vec<u8> {
        let mut out = Vec::new();
        let mut hs = OpenSecClientHandshake::new(&os_entropy(), None);
        let _ = hs.poll(0, &mut out);
        out.into_iter()
            .next()
            .expect("an OpenSec client opens with a hello")
    }

    /// The two start-up lines this zone logs, from the same function the
    /// transport logs them from.
    fn lines(zone: &mut Zone) -> [String; 2] {
        let (registry, policy) = zone_mechanisms(zone).expect("mechanisms");
        startup_lines(&registry, &policy)
    }

    /// The two start-up lines of an unset zone, word for word.
    #[test]
    fn the_two_startup_lines_are_exact() {
        let mut z = zone("");
        assert_eq!(
            lines(&mut z),
            [
                "Mechanisms supported: None, Subspace, OpenSecurity",
                "Mechanisms enabled on this zone: Subspace, OpenSecurity",
            ]
        );
    }

    /// `[OpenSec] Enabled = 0` and `Security:AllowUnencrypted` change the
    /// second line and only the second line: what the program can speak does
    /// not depend on what the operator switched on.
    #[test]
    fn the_enabled_line_follows_the_two_switches() {
        let supported = "Mechanisms supported: None, Subspace, OpenSecurity";
        for (conf, enabled) in [
            (
                "[ OpenSec ]\nEnabled = 0\n",
                "Mechanisms enabled on this zone: Subspace",
            ),
            (
                "[ Security ]\nAllowUnencrypted = 1\n",
                "Mechanisms enabled on this zone: None, Subspace, OpenSecurity",
            ),
            (
                "[ OpenSec ]\nEnabled = 0\n[ Security ]\nAllowUnencrypted = 1\n",
                "Mechanisms enabled on this zone: None, Subspace",
            ),
        ] {
            let mut z = zone(conf);
            assert_eq!(lines(&mut z), [supported, enabled], "{conf:?}");
        }
    }

    /// With OpenSec switched off, nothing about it is set up: no key is
    /// recorded and a real OpenSec hello makes no connection, as before the
    /// registry. With it on, the same hello is taken by OpenSec.
    #[test]
    fn opensec_switched_off_is_not_served() {
        let hello = opensec_hello();

        let mut z = zone("");
        let mut t = SubspaceTransport::new(&mut z);
        assert!(z.res.get::<crate::opensec::ServerPublicKey>().is_some());
        assert_eq!(t.offer(&hello), Some("opensec"));

        let mut z = zone("[ OpenSec ]\nEnabled = 0\n");
        let mut t = SubspaceTransport::new(&mut z);
        assert!(z.res.get::<crate::opensec::ServerPublicKey>().is_none());
        assert_eq!(t.offer(&hello), None);
        assert_eq!(t.connections(), 0);
        let mut z = zone("[ OpenSec ]\nEnabled = 0\n");
        let mut t = SubspaceTransport::new(&mut z);
        assert_eq!(t.offer(&request(0x8001_0000)), Some("vie"));
    }

    /// One `register_mechanism` call in a module's `load`, and nothing else:
    /// the dummy is named in both lines in its place by strength, and its
    /// opening is taken by its acceptor. Switching OpenSec off does not switch
    /// it off.
    #[test]
    fn a_mid_ranked_mechanism_registered_once_is_served() {
        let mut z = zone_with_dummy("");
        assert_eq!(
            lines(&mut z),
            [
                "Mechanisms supported: None, Subspace, Dummy, OpenSecurity",
                "Mechanisms enabled on this zone: Subspace, Dummy, OpenSecurity",
            ]
        );
        let mut z = zone_with_dummy("");
        let mut t = SubspaceTransport::new(&mut z);
        assert_eq!(t.offer(&DUMMY_HELLO), Some("dummy"));

        let mut z = zone_with_dummy("[ OpenSec ]\nEnabled = 0\n");
        assert_eq!(
            lines(&mut z)[1],
            "Mechanisms enabled on this zone: Subspace, Dummy"
        );
        let mut z = zone_with_dummy("[ OpenSec ]\nEnabled = 0\n");
        let mut t = SubspaceTransport::new(&mut z);
        assert_eq!(t.offer(&DUMMY_HELLO), Some("dummy"));
    }

    /// A module cannot take a name that is already taken, by a built-in
    /// mechanism or by an earlier module, and the refusal comes back to the
    /// module's `load` rather than surfacing later.
    #[test]
    fn a_clashing_mechanism_is_refused_when_registered() {
        let mut z = zone("");
        assert!(register_mechanism(&mut z, Owner(1), dummy()).is_ok());
        assert!(matches!(
            register_mechanism(&mut z, Owner(2), dummy()),
            Err(MechanismError::DuplicateId(_))
        ));
        assert!(matches!(
            register_mechanism(&mut z, Owner(2), Mechanism::vie()),
            Err(MechanismError::DuplicateId(_))
        ));
        let named_like_opensec = Mechanism::new(
            "other@example.org",
            "OpenSecurity",
            Era::Modern,
            2500,
            Selection::Automatic,
        )
        .expect("a valid description");
        assert!(matches!(
            register_mechanism(&mut z, Owner(2), named_like_opensec),
            Err(MechanismError::DuplicateDisplayName(_))
        ));
        // The one that was accepted is the only one the zone has.
        let mut t = SubspaceTransport::new(&mut z);
        assert_eq!(t.offer(&DUMMY_HELLO), Some("dummy"));
    }

    /// A module's mechanism leaves with the module: unloaded, it is gone from
    /// the lines; loaded again, it registers again rather than clashing with
    /// itself.
    #[test]
    fn a_module_can_be_unloaded_and_loaded_again() {
        let mut z = zone_with_dummy("");
        crate::module::unload(&mut z, "dummy").expect("unload");
        assert_eq!(
            lines(&mut z)[0],
            "Mechanisms supported: None, Subspace, OpenSecurity"
        );
        crate::module::load(&mut z, "dummy").expect("load again");
        assert_eq!(
            lines(&mut z)[0],
            "Mechanisms supported: None, Subspace, Dummy, OpenSecurity"
        );
    }

    /// Which acceptor takes a datagram: the first in `chain` that accepts it,
    /// by the name of the cipher it gives.
    fn winner(chain: &[Box<dyn Acceptor>], datagram: &[u8]) -> Option<&'static str> {
        chain
            .iter()
            .find_map(|a| a.accept(0, datagram, &mut Vec::new()))
            .map(|accepted| match accepted {
                Accept::Established(c) => c.name(),
                Accept::Pending(_) => "pending",
            })
    }

    /// **The order the acceptors are asked in changed shape, and that changes
    /// no outcome.** Before the registry the order was written out by hand:
    /// whatever modules registered, then OpenSec, then VIE, then `none` when
    /// switched on. Now it is read from the mechanisms' strengths: OpenSec,
    /// then a module mechanism ranked below it, then VIE, then `none`. The
    /// only pair whose order is swapped is OpenSec and that module mechanism.
    ///
    /// Order only decides anything where two acceptors would both take the same
    /// datagram. OpenSec's takes only a well-formed `00 01 'O' 'S' 'E' 'C'`
    /// hello, so the swap cannot matter for a module mechanism that does not
    /// itself take OpenSec's hello; that is the limit of what this proves, and
    /// a module that did take it would now lose it to OpenSec, the stronger.
    ///
    /// Both chains are asked about the same datagrams, chosen to sit on every
    /// boundary between the acceptors, and must give the same answer for each.
    /// Then the zone's real transport is asked too, to show it is the
    /// registry's chain that it uses.
    #[test]
    fn the_asking_order_cannot_change_which_acceptor_takes_a_datagram() {
        let conf = "[ Security ]\nAllowUnencrypted = 1\n";
        let mut z = zone_with_dummy(conf);
        let (registry, policy) = zone_mechanisms(&mut z).expect("mechanisms");
        let order: Vec<&str> = policy.order().iter().map(MechanismId::as_str).collect();
        assert_eq!(order, ["opensec", "dummy@example.org", "vie"]);
        let deliberate: Vec<&str> = policy
            .deliberate()
            .iter()
            .map(MechanismId::as_str)
            .collect();
        assert_eq!(deliberate, ["none"]);
        let now = policy.acceptors(&registry).expect("acceptors");

        let e = os_entropy();
        let before: Vec<Box<dyn Acceptor>> = vec![
            Box::new(DummyAcceptor),
            Box::new(
                OpenSecAcceptor::new(Some(ServerKey::generate(&e)), e).with_client_identities(),
            ),
            Box::new(VieAcceptor {
                accept_continuum: false,
            }),
            Box::new(NullAcceptor),
        ];

        let hello = opensec_hello();
        let mut bad_hello = hello.clone();
        bad_hello.truncate(hello.len() - 1);
        let mut continuum_proto = request(0x8001_0000);
        continuum_proto[6] = 0x11;
        let cases: Vec<(&str, Vec<u8>, Option<&str>)> = vec![
            ("a SubSpace request", request(0x8001_0000), Some("vie")),
            ("a request for no encryption", request(0), Some("none")),
            (
                "a request on the Continuum protocol",
                continuum_proto,
                Some("none"),
            ),
            ("a `00 11` opening", vec![0, 0x11, 1, 2, 3, 4, 1, 0], None),
            ("a real OpenSec hello", hello, Some("opensec")),
            ("an OpenSec hello cut short", bad_hello, None),
            // passes both OpenSec's magic test and the SubSpace length test
            (
                "a SubSpace request whose key spells OSEC",
                vec![0, 1, b'O', b'S', b'E', b'C', 1, 0],
                Some("vie"),
            ),
            ("the dummy's opening", DUMMY_HELLO.to_vec(), Some("dummy")),
            (
                "the dummy's opening, longer",
                [&DUMMY_HELLO[..], &[0, 0]].concat(),
                None,
            ),
            ("a bare `00 01`", vec![0, 1], None),
            ("a reliable packet", vec![0, 3, 0, 0, 0, 0, 0x0e], None),
            ("junk", vec![0xff; 12], None),
        ];
        for (what, datagram, expected) in &cases {
            let before_got = winner(&before, datagram);
            let now_got = winner(&now, datagram);
            assert_eq!(now_got, before_got, "{what}: the two orders disagree");
            assert_eq!(now_got, *expected, "{what}");

            let mut z = zone_with_dummy(conf);
            let mut t = SubspaceTransport::new(&mut z);
            assert_eq!(
                t.offer(datagram),
                *expected,
                "{what}: through the transport"
            );
        }
    }
}
