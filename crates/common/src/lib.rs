//! Shared process-wide helpers for machined-rs.

use tracing_subscriber::EnvFilter;

/// Initialise structured logging to the console.
///
/// Reads the `RUST_LOG` env filter; defaults to `info` when unset. Safe to call
/// once at process start. Calling twice is a no-op (the second `try_init` fails
/// and is ignored).
pub fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
