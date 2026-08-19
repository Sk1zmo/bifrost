//! Logging setup.
//!
//! Controlled by `BIFROST_LOG` (falling back to `RUST_LOG`), defaulting to
//! `info`. Idempotent, because the FFI entry point and the CLI can both
//! reasonably call it.

use std::sync::Once;

use tracing_subscriber::EnvFilter;

static INIT: Once = Once::new();

/// Initialise logging to stderr. Safe to call more than once.
pub fn init() {
    init_with_default("info");
}

/// As [`init`], but with an explicit fallback level.
pub fn init_with_default(default_level: &str) {
    INIT.call_once(|| {
        let filter = std::env::var("BIFROST_LOG")
            .or_else(|_| std::env::var("RUST_LOG"))
            .ok()
            .and_then(|raw| EnvFilter::try_new(raw).ok())
            .unwrap_or_else(|| EnvFilter::new(default_level));

        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            // Logs go to stderr so the terminal dashboard owns stdout and a
            // redirect of stdout does not swallow diagnostics.
            .with_writer(std::io::stderr)
            .try_init();
    });
}

/// Initialise logging for the terminal dashboard.
///
/// The TUI takes over the whole screen, so log lines must not be drawn into
/// it; they are written to `~/.bifrost/bifrost.log` instead.
pub fn init_for_tui(log_path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    INIT.call_once(|| {
        let filter = std::env::var("BIFROST_LOG")
            .or_else(|_| std::env::var("RUST_LOG"))
            .ok()
            .and_then(|raw| EnvFilter::try_new(raw).ok())
            .unwrap_or_else(|| EnvFilter::new("info"));

        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(file)
            .try_init();
    });

    Ok(())
}
