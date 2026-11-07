//! Log setup: `tracing` with human-readable or JSON output.

use tracing_subscriber::EnvFilter;

/// Installs the global subscriber. `filter` uses `RUST_LOG` syntax and
/// defaults to `RUST_LOG`, then `info`. Returns false if a subscriber was
/// already installed (e.g. by a test harness).
pub fn init(json: bool, filter: Option<&str>) -> bool {
    let filter = match filter {
        Some(f) => EnvFilter::new(f),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()));
    if json {
        builder.json().try_init().is_ok()
    } else {
        builder.try_init().is_ok()
    }
}
