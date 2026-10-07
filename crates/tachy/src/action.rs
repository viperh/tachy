use serde::{Deserialize, Serialize};
use strum::Display;

/// Messages passed between the event loop, [`App`](crate::app::App) and every
/// [`Component`](crate::components::Component).
///
/// Add your own variants here; components return them from `update` /
/// `handle_key_event` and `App::handle_actions` dispatches them.
#[derive(Debug, Clone, PartialEq, Eq, Display, Serialize, Deserialize)]
pub enum Action {
    Tick,
    Render,
    Resize(u16, u16),
    Suspend,
    Resume,
    Quit,
    ClearScreen,
    Error(String),
    Help,
}
