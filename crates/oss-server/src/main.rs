//! `oss-server`: runs a zone. With no arguments it runs the bundled default
//! zone; `--zone <dir>` runs an ASSS-style zone directory instead.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Open SubSpace zone server.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Zone directory (containing conf/, arenas/, maps/). Without it the
    /// bundled default zone is used.
    #[arg(long)]
    zone: Option<PathBuf>,
    /// Write the bundled default zone to this directory and exit.
    #[arg(long, value_name = "DIR", conflicts_with = "zone")]
    init_zone: Option<PathBuf>,
    /// Listen address for the game port (ping uses port + 1).
    #[arg(long)]
    bind: Option<SocketAddr>,
    /// Log as JSON lines.
    #[arg(long)]
    json: bool,
    /// Log filter (RUST_LOG syntax), e.g. "info,net=debug".
    #[arg(long)]
    log: Option<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    oss_server::logging::init(args.json, args.log.as_deref());
    if let Some(dir) = &args.init_zone {
        return match oss_server::default_zone::write_to(dir) {
            Ok(()) => {
                tracing::info!(dir = %dir.display(), "wrote the default zone");
                ExitCode::SUCCESS
            }
            Err(e) => {
                tracing::error!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    let mut builder = match &args.zone {
        Some(dir) => {
            tracing::info!(version = env!("CARGO_PKG_VERSION"), zone = %dir.display(), "oss-server starting");
            oss_server::runtime::ServerBuilder::new(dir)
        }
        None => {
            tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                "oss-server starting with the bundled default zone"
            );
            oss_server::runtime::ServerBuilder::default_zone()
        }
    };
    if let Some(b) = args.bind {
        builder = builder.bind(b);
    }
    match builder.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}
