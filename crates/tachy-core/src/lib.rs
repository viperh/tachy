//! Domain logic for tachy, with no knowledge of the terminal.
//!
//! Everything here is UI-agnostic: no `ratatui`, `crossterm` or `clap`. The
//! `tachy` crate owns rendering, key handling and the event loop; this crate
//! owns file access, parsing, views, queries and background work, so it can be
//! unit tested without a terminal.

use std::{io, path::PathBuf};

use thiserror::Error;

pub mod cache;
pub mod column;
pub mod dialect;
pub mod exec;
pub mod export;
pub mod filter;
pub mod index;
pub mod jobs;
pub mod parse;
pub mod query;
pub mod sample;
pub mod search;
pub mod size;
pub mod sort;
pub mod source;
pub mod spool;
pub mod stats;
pub mod text;
pub mod types;
pub mod view;

/// Errors produced by the core.
///
/// The `tachy` crate converts these into `color_eyre` reports at the boundary,
/// which is why this enum carries no formatting or reporting concerns of its own.
#[derive(Debug, Error)]
pub enum Error {
    /// The core was asked to do something its current state does not allow.
    #[error("invalid state transition: {0}")]
    InvalidState(String),

    /// An I/O operation on `path` failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The operation was cancelled before it finished.
    #[error("cancelled")]
    Cancelled,
}

/// Convenience alias used throughout this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
