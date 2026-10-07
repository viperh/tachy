use ratatui::{
    prelude::*,
    widgets::{Block, Paragraph},
};
use tokio::sync::mpsc::UnboundedSender;
use crate::theme::Theme;

use super::Component;
use crate::{action::Action, config::Config};

/// The default screen. Use it as the shape to copy when adding components.
#[derive(Default)]
pub struct Home {
    command_tx: Option<UnboundedSender<Action>>,
    config: Config,
}

impl Home {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Component for Home {
    fn register_action_handler(&mut self, tx: UnboundedSender<Action>) -> color_eyre::Result<()> {
        self.command_tx = Some(tx);
        Ok(())
    }

    fn register_config_handler(&mut self, config: Config) -> color_eyre::Result<()> {
        self.config = config;
        Ok(())
    }

    fn update(&mut self, action: Action) -> color_eyre::Result<Option<Action>> {
        match action {
            Action::Tick => {}
            Action::Render => {}
            _ => {}
        }
        Ok(None)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) -> color_eyre::Result<()> {
        let [main, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(3)]).areas(area);

        frame.render_widget(
            Paragraph::new("TEST")
                .block(Block::bordered().title("TEST AAA"))
                .fg(Color::Rgb(0, 205, 205)),
            main,
        );

        frame.render_widget(
            Paragraph::new("Ctrl + q - Quit")
                .block(Block::bordered())
                .fg(Theme::rgb().),
            footer,
        );

        Ok(())
    }
}
