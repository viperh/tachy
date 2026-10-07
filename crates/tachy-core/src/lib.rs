//! Domain logic for the application, with no knowledge of the terminal.
//!
//! Keep everything here UI-agnostic. The `tachy` crate owns rendering, key
//! handling and the event loop; this crate owns state and the rules that
//! govern it, so it can be unit tested without spawning a terminal.
//!
//! Replace [`Core`] and [`enum@Error`] with your own domain types.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors produced by the core.
///
/// The `tachy` crate converts these into `color_eyre` reports at the boundary,
/// which is why this enum carries no formatting or reporting concerns of its own.
#[derive(Debug, Error)]
pub enum Error {
    /// The core was asked to do something its current state does not allow.
    #[error("invalid state transition: {0}")]
    InvalidState(String),
}

/// Convenience alias used throughout this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The application's domain state.
///
/// This placeholder exists so the `tachy` -> `tachy-core` seam is wired and
/// compiled by CI from the first commit. Swap the contents for your own model.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Core {
    ticks: u64,
}

impl Core {
    /// Build the initial domain state.
    pub fn new() -> Result<Self> {
        Ok(Self::default())
    }

    /// Advance the domain clock by one tick.
    pub fn tick(&mut self) {
        self.ticks = self.ticks.saturating_add(1);
    }

    /// Number of ticks observed so far.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn new_core_starts_at_zero_ticks() -> Result<()> {
        assert_eq!(Core::new()?.ticks(), 0);
        Ok(())
    }

    #[test]
    fn tick_advances_the_counter() -> Result<()> {
        let mut core = Core::new()?;
        core.tick();
        core.tick();
        assert_eq!(core.ticks(), 2);
        Ok(())
    }
}
