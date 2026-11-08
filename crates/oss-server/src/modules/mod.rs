//! Built-in modules and the default module set.
//!
//! Each module is registered under its own name and its ASSS name(s), so an
//! ASSS `conf/modules.conf` loads the equivalent modules unchanged.

pub mod arenaman;
pub mod auth_file;
pub mod balls;
pub mod banners;
pub mod billing_ssc;
pub mod bricks;
pub mod capman;
pub mod chat;
pub mod chatnet;
pub mod clientset;
pub mod cmdman;
pub mod core;
pub mod directory;
pub mod fg_turf;
pub mod fg_wz;
pub mod flagcore;
pub mod freqowners;
pub mod game;
pub mod jackpot;
pub mod koth;
pub mod lagdata;
pub mod mapdata;
pub mod mapnewsdl;
pub mod obscene;
pub mod periodic;
pub mod persist;
pub mod playercmd;
pub mod points_goal;
pub mod redirect;
pub mod security;
pub mod staffcmd;
pub mod stats;

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
    "obscene",
    "game",
    "mapdata",
    "mapnewsdl",
    "persist",
    "stats",
    "basicstats",
    "auth_file",
    "security",
    "points_kill",
    "flagcore",
    "balls",
    "bricks",
    "banners",
    "koth",
    "freqowners",
    "redirect",
    "lagdata",
    "lagaction",
    "periodic",
    "points_periodic",
    "points_goal",
    "fg_wz",
    "fg_turf",
    "jackpot",
    "points_flag",
    "playercmd",
    "staffcmd",
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
    c.register("chatnet", &[], || chatnet::ChatNetModule);
    c.register("directory", &[], || directory::DirectoryModule);
    c.register("obscene", &[], || obscene::ObsceneModule);
    c.register("game", &[], || game::GameModule);
    c.register("mapdata", &[], || mapdata::MapDataModule);
    c.register("mapnewsdl", &[], || mapnewsdl::MapNewsDlModule);
    c.register("stats", &["scoring:stats"], || stats::StatsModule);
    c.register("basicstats", &["scoring:basicstats"], || {
        stats::BasicStatsModule
    });
    c.register("points_kill", &["scoring:points_kill"], || {
        stats::PointsKillModule
    });
    c.register("security", &["security:security"], || {
        security::SecurityModule
    });
    c.register("flagcore", &[], || flagcore::FlagCoreModule);
    c.register("balls", &[], || balls::BallsModule);
    c.register("bricks", &[], || bricks::BricksModule);
    c.register("banners", &[], || banners::BannersModule);
    c.register("billing_ssc", &[], || billing_ssc::BillingSscModule);
    c.register("koth", &[], || koth::KothModule);
    c.register("freqowners", &[], || freqowners::FreqOwnersModule);
    c.register("redirect", &[], || redirect::RedirectModule);
    c.register("lagdata", &[], || lagdata::LagDataModule);
    c.register("lagaction", &[], || lagdata::LagActionModule);
    c.register("persist", &["scoring:persist"], || persist::PersistModule);
    c.register("periodic", &["scoring:periodic"], || {
        periodic::PeriodicModule
    });
    c.register("points_periodic", &["scoring:points_periodic"], || {
        periodic::PointsPeriodicModule
    });
    c.register("points_goal", &["scoring:points_goal"], || {
        points_goal::PointsGoalModule
    });
    c.register("fg_wz", &["<py> fg_wz"], || fg_wz::FgWzModule);
    c.register("fg_turf", &["<py> fg_turf"], || fg_turf::FgTurfModule);
    c.register("jackpot", &["scoring:jackpot"], || jackpot::JackpotModule);
    c.register("points_flag", &["scoring:points_flag"], || {
        jackpot::PointsFlagModule
    });
    c.register("auth_file", &[], || auth_file::AuthFileModule);
    c.register("playercmd", &[], || playercmd::PlayerCmdModule);
    c.register("staffcmd", &["admincmd"], || staffcmd::StaffCmdModule);
    c
}

/// The transport used when the builder isn't given one.
pub fn default_transport(zone: &mut Zone) -> Box<dyn Transport> {
    Box::new(crate::transport::SubspaceTransport::new(zone))
}

/// Accepts no connections.
pub struct NullTransport;

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
