//! UI components: one per screen region (spec §11, README §A3).
//!
//! `App` keeps each component in a named field and calls its `draw` with the
//! component's own region from [`layout::compute_layout`]. Components keep
//! only local UI state (input buffers, selections, scroll offsets); shared
//! state lives in [`AppState`].

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Rect, Size},
    style::Style,
    widgets::{Block, Paragraph},
};
use tokio::sync::mpsc::UnboundedSender;

use crate::{action::Action, config::Config, state::AppState, tui::Event};

pub mod dialogs;
pub mod help;
pub mod hints;
pub mod inspector;
pub mod jobs_drawer;
pub mod layout;
pub mod palette;
pub mod query_bar;
pub mod status;
pub mod table;
pub mod toast;
pub mod top_bar;

/// `Component` is a trait that represents a visual and interactive element of the user interface.
///
/// Implementors receive events, update their local state and shared
/// [`AppState`], and draw into the region `App` gives them.
pub trait Component {
    /// Register an action handler that can send actions for processing if necessary.
    fn register_action_handler(&mut self, tx: UnboundedSender<Action>) -> color_eyre::Result<()> {
        let _ = tx; // to appease clippy
        Ok(())
    }
    /// Register a configuration handler that provides configuration settings if necessary.
    fn register_config_handler(&mut self, config: Config) -> color_eyre::Result<()> {
        let _ = config; // to appease clippy
        Ok(())
    }
    /// Initialize the component with the terminal size if necessary.
    fn init(&mut self, area: Size) -> color_eyre::Result<()> {
        let _ = area; // to appease clippy
        Ok(())
    }
    /// Handle an incoming event and produce an action if necessary.
    fn handle_events(
        &mut self,
        event: Option<Event>,
        state: &AppState,
    ) -> color_eyre::Result<Option<Action>> {
        let action = match event {
            Some(Event::Key(key_event)) => self.handle_key_event(key_event, state)?,
            Some(Event::Mouse(mouse_event)) => self.handle_mouse_event(mouse_event, state)?,
            _ => None,
        };
        Ok(action)
    }
    /// Handle a key event and produce an action if necessary.
    fn handle_key_event(
        &mut self,
        key: KeyEvent,
        state: &AppState,
    ) -> color_eyre::Result<Option<Action>> {
        let _ = (key, state); // to appease clippy
        Ok(None)
    }
    /// Handle a mouse event and produce an action if necessary.
    fn handle_mouse_event(
        &mut self,
        mouse: MouseEvent,
        state: &AppState,
    ) -> color_eyre::Result<Option<Action>> {
        let _ = (mouse, state); // to appease clippy
        Ok(None)
    }
    /// Update local and shared state for an action. May return a follow-up action.
    fn update(
        &mut self,
        action: &Action,
        state: &mut AppState,
    ) -> color_eyre::Result<Option<Action>> {
        let _ = (action, state); // to appease clippy
        Ok(None)
    }
    /// Draw the component into `area`, its own region of the screen.
    fn draw(&mut self, frame: &mut Frame, area: Rect, state: &AppState) -> color_eyre::Result<()>;
}

/// Placeholder drawing: fills `area` with `background` and writes `name` in
/// `theme.dim()`. Replaced region by region by the M1+ tasks.
fn draw_placeholder(
    frame: &mut Frame,
    area: Rect,
    background: Style,
    name: &str,
    state: &AppState,
) {
    frame.render_widget(Block::new().style(background), area);
    frame.render_widget(Paragraph::new(name).style(state.theme.dim()), area);
}

/// Writes `s` at `(x, y)` in `style`, clipped to the buffer. Returns the
/// column after the last cell written.
pub fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) -> u16 {
    let area = buf.area;
    if y < area.top() || y >= area.bottom() || x >= area.right() {
        return x;
    }
    buf.set_stringn(x, y, s, usize::from(area.right() - x), style)
        .0
}
