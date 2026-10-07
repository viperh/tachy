//! Entry point.
//!
//! The binary owns everything terminal-related; domain logic lives in the
//! `tachy-core` crate so it stays testable without a TTY.

use crate::{app::App, cli::Cli, config::Config, settings::Settings, theme::ColorSupport};

mod action;
mod app;
mod cli;
mod clipboard;
mod commands;
mod components;
mod config;
mod errors;
mod goto;
mod help_model;
mod input;
mod jobs;
mod keymap;
mod logging;
mod mode;
mod msg;
mod path_complete;
mod query_history;
mod rate;
mod search_ui;
mod settings;
#[cfg(unix)]
mod sigbus;
mod spool;
mod state;
mod tab;
mod theme;
mod toast;
mod tui;
mod views_store;
mod watch;

/// Exit codes (spec §3): `0` normal exit, `1` runtime error (returning `Err`
/// from `main`), `2` usage error (clap, or [`Cli::parse_and_validate`]).
#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    crate::errors::init()?;
    crate::logging::init()?;

    let cli = Cli::parse_and_validate();
    let config = Config::new();
    let settings = Settings::resolve(&cli, &config)?;
    // Once, at startup (§14 "Fallback", `NO_COLOR`).
    let color = ColorSupport::detect();
    tracing::info!(?color, "colour support");
    // §16: a file truncated while mapped raises SIGBUS. Save the terminal
    // attributes before raw mode, so the handler can restore them.
    #[cfg(unix)]
    {
        sigbus::save_termios();
        let files: Vec<&std::path::Path> = settings
            .files
            .iter()
            .filter(|p| p.as_os_str() != "-")
            .map(|p| p.as_path())
            .collect();
        sigbus::set_message(&files);
        if let Err(e) = sigbus::install() {
            tracing::warn!("cannot install the SIGBUS handler: {e}");
        }
    }
    let mut app = App::new(config, settings, color)?;

    #[cfg(debug_assertions)]
    let result = tokio::select! {
        result = app.run() => result,
        never = debug_panic() => match never {},
    };
    #[cfg(not(debug_assertions))]
    let result = app.run().await;

    // `run` may have returned early through `?` before leaving the terminal;
    // restore it so color_eyre prints the report on the normal screen.
    let _ = crate::tui::restore_terminal();
    if let Err(e) = &result {
        // Also in the log: stderr may be gone (the terminal hung up).
        tracing::error!("exiting with an error: {e:?}");
    }

    // A read of stdin (`-`) that is still blocked would keep the runtime
    // from shutting down: drop the app (closing the tabs deletes their temp
    // files), then exit without waiting for it.
    if app.stdin_reading() {
        drop(app);
        if let Err(err) = result {
            eprintln!("Error: {err:?}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    result
}

/// Debug builds only: `TACHY_DEBUG_PANIC=main` panics on the main task and
/// `TACHY_DEBUG_PANIC=blocking` inside `spawn_blocking`, half a second after
/// the UI started, to check that the panic hook restores the terminal (§16).
/// Otherwise never completes.
#[cfg(debug_assertions)]
async fn debug_panic() -> std::convert::Infallible {
    use std::time::Duration;

    let mode = std::env::var("TACHY_DEBUG_PANIC").unwrap_or_default();
    if mode == "main" || mode == "blocking" {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    match mode.as_str() {
        "main" => panic!("TACHY_DEBUG_PANIC=main"),
        "blocking" => {
            // The hook exits the process from the blocking thread.
            let _ = tokio::task::spawn_blocking(|| panic!("TACHY_DEBUG_PANIC=blocking")).await;
        }
        _ => {}
    }
    std::future::pending().await
}
