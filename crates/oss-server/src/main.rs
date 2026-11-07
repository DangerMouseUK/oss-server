//! `oss-server`: runs a zone from an ASSS-style zone directory.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Open SubSpace zone server.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Zone directory (containing conf/, arenas/, maps/).
    #[arg(default_value = ".")]
    zone: PathBuf,
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
    tracing::info!(version = env!("CARGO_PKG_VERSION"), zone = %args.zone.display(), "oss-server starting");
    let mut builder = oss_server::runtime::ServerBuilder::new(&args.zone);
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
