//! `directory`: registers the zone with directory servers (ASSS
//! `directory.c`), so it shows in client zone lists.
//!
//! Every 60 seconds (first after 10) the zone sends one plain UDP datagram
//! ([`oss_proto::directory::ZoneUpdate`]) to each `Directory:Server1` ..
//! `Server9` (port `Directory:Port1..9`, default `Directory:Port`, 4991). It
//! carries the game port, the player count, `Directory:Name`,
//! `Directory:Description` and `Directory:Password` (default `cane`). The
//! datagram goes out from an ephemeral port: directories take the game port
//! from the payload and ping the registered port + 1 at the sender's address,
//! so the zone's ping responder must be reachable there.
//!
//! A zone with several listen sockets sends one datagram per socket. A
//! virtual server (`Listen:ConnectAs`) reads `Name` and `Description` from a
//! `[Directory-<connectas>]` section first, and reports only its own
//! population (the players in arenas named after it).
//!
//! Nothing is sent unless servers are configured; the bundled zone lists
//! none.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

use oss_proto::directory::ZoneUpdate;
use oss_proto::wire::FixedStr;

use crate::config::to_latin1;
use crate::module::{Module, ModuleError};
use crate::player::{ClientKind, PlayerStatus};
use crate::registry::Owner;
use crate::zone::Zone;

/// Version number directories require (ASSS sends 134, not its own).
pub const VERSION: u32 = 134;
/// Longest description sent, without its NUL (ASSS: 386-byte field).
pub const MAX_DESCRIPTION: usize = 385;
/// Longest password directory servers accept.
pub const MAX_DIR_PASSWORD: usize = 15;

fn fixed<const N: usize>(s: &str) -> FixedStr<N> {
    let b = to_latin1(s);
    FixedStr::new(&b[..b.len().min(N - 1)])
}

/// The registration datagram ASSS's `init_data` and `SendUpdates` build.
pub fn update_packet(
    port: u16,
    players: u16,
    name: Option<&str>,
    description: Option<&str>,
    password: Option<&str>,
) -> Vec<u8> {
    let mut description = to_latin1(description.unwrap_or("<no description provided>"));
    description.truncate(MAX_DESCRIPTION);
    let mut out = Vec::with_capacity(ZoneUpdate::FIXED_LEN + description.len() + 1);
    ZoneUpdate {
        ip: 0,
        port,
        players,
        // ASSS: "always keep scores"
        score_keeping: 1,
        version: VERSION,
        name: fixed(name.unwrap_or("<no name provided>")),
        password: fixed(password.unwrap_or("cane")),
        description,
    }
    .encode(&mut out);
    out
}

struct Directory {
    socket: UdpSocket,
    servers: Vec<SocketAddr>,
}

fn servers(zone: &Zone) -> Vec<SocketAddr> {
    let g = zone.cfg.global();
    let default_port = u16::try_from(g.get_int("Directory", "Port", 4991)).unwrap_or(4991);
    let mut out = Vec::new();
    for i in 1..10 {
        let Some(name) = g.get("Directory", &format!("Server{i}")) else {
            continue;
        };
        let port =
            u16::try_from(g.get_int("Directory", &format!("Port{i}"), i32::from(default_port)))
                .unwrap_or(default_port);
        match (name.trim(), port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.find(SocketAddr::is_ipv4))
        {
            Some(a) => {
                tracing::info!(target: "directory", "using '{name}' at {a} as a directory server");
                out.push(a);
            }
            None => tracing::warn!(target: "directory", "can't resolve directory server '{name}'"),
        }
    }
    out
}

/// Players counted in the registration: everyone playing except fakes, or
/// for a virtual server those in arenas named after it (ASSS
/// `GetLDPopulation`).
pub fn population(zone: &Zone, connect_as: Option<&str>) -> u16 {
    let n = zone
        .players
        .iter()
        .filter(|p| p.status == PlayerStatus::Playing && p.kind != ClientKind::Fake)
        .filter(|p| match connect_as {
            None => true,
            Some(c) => p
                .arena
                .and_then(|a| zone.arenas.get(a))
                .is_some_and(|a| a.basename == c),
        })
        .count();
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// The registration datagram for each listen socket (ASSS `init_data` +
/// `SendUpdates`).
pub fn packets(zone: &Zone) -> Vec<Vec<u8>> {
    let g = zone.cfg.global();
    crate::net::Listeners::of(zone)
        .iter()
        .map(|l| {
            let sec = l
                .connect_as
                .as_ref()
                .map_or_else(|| "Directory".to_owned(), |c| format!("Directory-{c}"));
            let pick = |key: &str| g.get(&sec, key).or_else(|| g.get("Directory", key));
            update_packet(
                l.addr.port(),
                population(zone, l.connect_as.as_deref()),
                pick("Name"),
                pick("Description"),
                g.get("Directory", "Password"),
            )
        })
        .collect()
}

fn send_updates(zone: &mut Zone) -> bool {
    let Some(d) = zone.res.get::<Directory>() else {
        return false;
    };
    if d.servers.is_empty() {
        return true;
    }
    tracing::trace!(target: "directory", "sending information to directory servers");
    for pkt in packets(zone) {
        for s in &d.servers {
            if let Err(e) = d.socket.send_to(&pkt, s) {
                tracing::debug!(target: "directory", %s, "send failed: {e}");
            }
        }
    }
    true
}

/// The `directory` module.
pub struct DirectoryModule;

impl Module for DirectoryModule {
    fn load(&self, zone: &mut Zone, me: Owner) -> Result<(), ModuleError> {
        // an ephemeral port: directories read the game port from the payload
        let socket = UdpSocket::bind(("0.0.0.0", 0)).map_err(|e| ModuleError::Failed {
            module: "directory".into(),
            reason: e.to_string(),
        })?;
        let pw = zone
            .cfg
            .global()
            .get("Directory", "Password")
            .unwrap_or("cane");
        if to_latin1(pw).len() > MAX_DIR_PASSWORD {
            tracing::warn!(target: "directory", "Directory:Password is longer than {MAX_DIR_PASSWORD} characters; directory servers will reject the zone");
        }
        let servers = servers(zone);
        for l in crate::net::Listeners::of(zone) {
            tracing::info!(target: "directory", port = l.addr.port(), connect_as = ?l.connect_as, "registering listen socket");
        }
        zone.res.insert(Directory { socket, servers });
        zone.set_timer(me, 1000, 6000, send_updates);
        Ok(())
    }

    fn unload(&self, zone: &mut Zone, _me: Owner) {
        zone.res.remove::<Directory>();
    }
}
