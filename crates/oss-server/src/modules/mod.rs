//! Built-in modules and the default module set.
//!
//! Each module is registered under its own name and its ASSS name(s), so an
//! ASSS `conf/modules.conf` loads the equivalent modules unchanged.

pub mod arenaman;
pub mod auth_file;
pub mod capman;
pub mod chat;
pub mod clientset;
pub mod cmdman;
pub mod core;
pub mod game;
pub mod playercmd;

use crate::module::ModuleCatalog;
use crate::net::Transport;
use crate::zone::Zone;

/// Modules loaded when the zone has no `conf/modules.conf` and the builder
/// was given no list.
pub const DEFAULT_MODULES: &[&str] = &[
    "core",
    "arenaman",
    "capman",
    "clientset",
    "cmdman",
    "chat",
    "game",
    "auth_file",
    "playercmd",
];

/// Every module shipped with this crate, under its own name and its ASSS name.
pub fn builtin_catalog() -> ModuleCatalog {
    let mut c = ModuleCatalog::default();
    // "core" is also in module::BUILT_INTO_CORE; registering it here makes
    // the lifecycle module load when modules.conf lists it.
    c.register("core", &[], || core::CoreModule);
    c.register("arenaman", &[], || arenaman::ArenaManModule);
    c.register("capman", &[], || capman::CapManModule);
    c.register("clientset", &[], || clientset::ClientSetModule);
    c.register("cmdman", &[], || cmdman::CmdManModule);
    c.register("chat", &[], || chat::ChatModule);
    c.register("game", &[], || game::GameModule);
    c.register("auth_file", &[], || auth_file::AuthFileModule);
    c.register("playercmd", &[], || playercmd::PlayerCmdModule);
    c
}

/// The transport used when the builder isn't given one.
pub fn default_transport(_zone: &mut Zone) -> Box<dyn Transport> {
    Box::new(NullTransport)
}

/// Accepts no connections (placeholder until the `oss-net` transport lands).
struct NullTransport;

impl Transport for NullTransport {
    fn on_datagram(
        &mut self,
        _: &mut Zone,
        _: &dyn crate::net::DatagramIo,
        from: std::net::SocketAddr,
        data: &[u8],
    ) {
        tracing::debug!(target: "net", %from, len = data.len(), "no transport; dropping datagram");
    }

    fn flush(&mut self, zone: &mut Zone, _: &dyn crate::net::DatagramIo) {
        let out = zone.take_outbox();
        zone.return_outbox(out);
    }

    fn tick(&mut self, _: &mut Zone, _: &dyn crate::net::DatagramIo) {}
}
