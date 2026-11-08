//! `oss-biller`: a local user database server for Open SubSpace zones.
//!
//! Point a zone at it with `Billing:IP`/`Billing:Port` in `global.conf` and
//! load `billing_ssc`. Meant for private and test zones.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::{Parser, ValueEnum};
use oss_biller::{Biller, BillerConfig, Registration, Store};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Reg {
    /// Unknown names are asked to register.
    Ask,
    /// Unknown names are registered at once.
    Auto,
    /// No new accounts.
    Closed,
}

#[derive(Parser)]
#[command(
    version,
    about = "Local user database server (SSC billing protocol) for Open SubSpace zones"
)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:1850")]
    bind: SocketAddr,
    /// SQLite database file.
    #[arg(long, default_value = "biller.db")]
    db: PathBuf,
    /// Password zones must send (their Billing:Password).
    #[arg(long)]
    password: Option<String>,
    /// What to do with names that have no account.
    #[arg(long, value_enum, default_value = "ask")]
    registration: Reg,
    /// Ban an account and exit.
    #[arg(long, value_name = "NAME")]
    ban: Option<String>,
    /// Lift a ban and exit.
    #[arg(long, value_name = "NAME")]
    unban: Option<String>,
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let store = match Store::open(&args.db) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("can't open {}: {e}", args.db.display());
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Some((name, banned)) = args
        .ban
        .as_ref()
        .map(|n| (n, true))
        .or(args.unban.as_ref().map(|n| (n, false)))
    {
        return match store.set_banned(name, banned) {
            Ok(true) => std::process::ExitCode::SUCCESS,
            Ok(false) => {
                eprintln!("no account named {name}");
                std::process::ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    let cfg = BillerConfig {
        password: args.password,
        registration: match args.registration {
            Reg::Ask => Registration::Ask,
            Reg::Auto => Registration::Auto,
            Reg::Closed => Registration::Closed,
        },
        ..BillerConfig::default()
    };
    let socket = match std::net::UdpSocket::bind(args.bind) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("can't bind {}: {e}", args.bind);
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::info!(target: "biller", "listening on {}", args.bind);
    let stop = Arc::new(AtomicBool::new(false));
    match oss_biller::run(Biller::new(store, cfg), &socket, &stop) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}
