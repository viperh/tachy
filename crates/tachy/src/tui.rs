//! Terminal lifecycle and the terminal event source (spec §4.1, §16).
//!
//! [`Tui`] owns the ratatui [`Terminal`](ratatui::Terminal) and a background
//! task that turns crossterm events into [`Event`]s. It never produces render
//! events: the app redraws only when something changed (see `App::run`).
//! Ticks (every 100 ms) are only produced while ticking is enabled with
//! [`Tui::set_ticking`], so an idle app blocks on terminal input alone.

use std::{
    io::{self, Stdout, stdout},
    ops::{Deref, DerefMut},
    time::Duration,
};

use color_eyre::eyre::eyre;
use crossterm::{
    cursor,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as CrosstermEvent, EventStream, KeyEvent, KeyEventKind, MouseEvent,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{Stream, StreamExt};
use ratatui::backend::{Backend, CrosstermBackend};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        watch,
    },
    task::JoinHandle,
    time::{MissedTickBehavior, interval},
};
use tokio_util::sync::CancellationToken;
use tracing::error;

/// Interval between [`Event::Tick`]s while ticking is enabled (§4.1).
pub const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Events produced by the terminal event task.
#[allow(dead_code)] // `Quit` and `Closed` are kept for completeness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    /// The event task started.
    Init,
    /// The terminal asked the app to quit.
    Quit,
    /// Reading a terminal event failed.
    Error,
    /// The event stream ended.
    Closed,
    /// 100 ms passed while ticking is enabled.
    Tick,
    FocusGained,
    FocusLost,
    /// Bracketed paste: the whole pasted text in one event.
    Paste(String),
    /// A key press or key repeat (releases are dropped).
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
}

/// Restores the terminal to its normal state: disables bracketed paste and
/// mouse capture, leaves the alternate screen, shows the cursor and disables
/// raw mode.
///
/// Needs no tokio runtime and no [`Tui`], so the panic hook can call it from
/// any thread. It does nothing when raw mode is off, so a second call is a
/// no-op.
pub fn restore_terminal() -> io::Result<()> {
    if !crossterm::terminal::is_raw_mode_enabled()? {
        return Ok(());
    }
    crossterm::execute!(
        stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    )?;
    crossterm::terminal::disable_raw_mode()
}

/// The terminal plus its event source.
///
/// Generic over the ratatui backend so tests can drive the app with a
/// `TestBackend`. Only the crossterm backend is ever [`enter`](Tui::enter)ed.
pub struct Tui<B: Backend = CrosstermBackend<Stdout>> {
    pub terminal: ratatui::Terminal<B>,
    /// The event task, spawned by [`start`](Tui::start). `None` until then.
    pub task: Option<JoinHandle<()>>,
    pub cancellation_token: CancellationToken,
    pub event_rx: UnboundedReceiver<Event>,
    pub event_tx: UnboundedSender<Event>,
    /// Whether the event task emits [`Event::Tick`]s.
    ticking: watch::Sender<bool>,
    pub mouse: bool,
    pub paste: bool,
}

impl Tui {
    /// A `Tui` on stdout. Touches neither the terminal nor the runtime until
    /// [`enter`](Tui::enter).
    pub fn new() -> color_eyre::Result<Self> {
        Ok(Self::with_backend(CrosstermBackend::new(stdout()))?)
    }

    /// Leaves the terminal and stops the process with `SIGTSTP` (unix).
    pub fn suspend(&mut self) -> color_eyre::Result<()> {
        self.exit()?;
        #[cfg(not(windows))]
        signal_hook::low_level::raise(signal_hook::consts::signal::SIGTSTP)?;
        Ok(())
    }
}

impl<B: Backend> Tui<B> {
    /// A `Tui` drawing to `backend`, with ticking disabled and no event task.
    pub fn with_backend(backend: B) -> Result<Self, B::Error> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ticking, _) = watch::channel(false);
        Ok(Self {
            terminal: ratatui::Terminal::new(backend)?,
            task: None,
            cancellation_token: CancellationToken::new(),
            event_rx,
            event_tx,
            ticking,
            mouse: false,
            paste: false,
        })
    }

    /// Capture the mouse (wheel scrolling, §1).
    pub fn mouse(mut self, mouse: bool) -> Self {
        self.mouse = mouse;
        self
    }

    /// Enable bracketed paste, so a paste arrives as one [`Event::Paste`].
    pub fn paste(mut self, paste: bool) -> Self {
        self.paste = paste;
        self
    }

    /// Turns the 100 ms tick on or off. Only wakes the event task when the
    /// value actually changes.
    pub fn set_ticking(&self, on: bool) {
        self.ticking.send_if_modified(|current| {
            let changed = *current != on;
            *current = on;
            changed
        });
    }

    /// Whether ticking is enabled.
    #[cfg(test)]
    pub fn is_ticking(&self) -> bool {
        *self.ticking.borrow()
    }

    /// (Re)starts the event task reading from the terminal.
    pub fn start(&mut self) {
        self.start_with(EventStream::new());
    }

    /// (Re)starts the event task reading from `stream`.
    fn start_with<S>(&mut self, stream: S)
    where
        S: Stream<Item = io::Result<CrosstermEvent>> + Send + Unpin + 'static,
    {
        self.cancel(); // Cancel any existing task
        self.cancellation_token = CancellationToken::new();
        self.task = Some(tokio::spawn(event_loop(
            stream,
            self.event_tx.clone(),
            self.cancellation_token.clone(),
            self.ticking.subscribe(),
        )));
    }

    /// Stops the event task, aborting it if it doesn't finish within 50 ms.
    pub fn stop(&mut self) {
        self.cancel();
        let Some(task) = self.task.take() else {
            return;
        };
        let mut counter = 0;
        while !task.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
            counter += 1;
            if counter > 50 {
                task.abort();
            }
            if counter > 100 {
                error!("Failed to abort task in 100 milliseconds for unknown reason");
                break;
            }
        }
    }

    /// Enters raw mode and the alternate screen, then starts the event task.
    pub fn enter(&mut self) -> color_eyre::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(stdout(), EnterAlternateScreen, cursor::Hide)?;
        if self.mouse {
            crossterm::execute!(stdout(), EnableMouseCapture)?;
        }
        if self.paste {
            crossterm::execute!(stdout(), EnableBracketedPaste)?;
        }
        self.start();
        Ok(())
    }

    /// Stops the event task and restores the terminal. Safe to call twice.
    pub fn exit(&mut self) -> color_eyre::Result<()> {
        self.stop();
        if crossterm::terminal::is_raw_mode_enabled()? {
            self.terminal.flush().map_err(|e| eyre!("{e}"))?;
        }
        restore_terminal()?;
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    /// The next terminal event. Never returns `None` while `self` is alive,
    /// because `self` holds a sender.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.event_rx.recv().await
    }
}

/// Forwards terminal events to `event_tx`, plus a tick every 100 ms while
/// `ticking` is `true`. While ticking is off, the loop only wakes for terminal
/// events, a change of `ticking` and cancellation.
async fn event_loop<S>(
    mut stream: S,
    event_tx: UnboundedSender<Event>,
    cancellation_token: CancellationToken,
    mut ticking: watch::Receiver<bool>,
) where
    S: Stream<Item = io::Result<CrosstermEvent>> + Unpin,
{
    let mut tick_interval = interval(TICK_INTERVAL);
    tick_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut ticking_on = *ticking.borrow_and_update();
    let mut watch_alive = true;

    // if this fails, then it's likely a bug in the calling code
    event_tx
        .send(Event::Init)
        .expect("failed to send init event");
    loop {
        let event = tokio::select! {
            _ = cancellation_token.cancelled() => {
                break;
            }
            changed = ticking.changed(), if watch_alive => {
                match changed {
                    Ok(()) => {
                        let on = *ticking.borrow_and_update();
                        if on && !ticking_on {
                            // First tick one interval from now.
                            tick_interval.reset();
                        }
                        ticking_on = on;
                    }
                    // The `Tui` is gone; it cancels us right after.
                    Err(_) => {
                        watch_alive = false;
                        ticking_on = false;
                    }
                }
                continue;
            }
            _ = tick_interval.tick(), if ticking_on => Event::Tick,
            crossterm_event = stream.next() => match crossterm_event {
                Some(Ok(event)) => match event {
                    // Repeats too, so holding `j` keeps scrolling.
                    CrosstermEvent::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        Event::Key(key)
                    }
                    CrosstermEvent::Mouse(mouse) => Event::Mouse(mouse),
                    CrosstermEvent::Resize(x, y) => Event::Resize(x, y),
                    CrosstermEvent::FocusLost => Event::FocusLost,
                    CrosstermEvent::FocusGained => Event::FocusGained,
                    CrosstermEvent::Paste(s) => Event::Paste(s),
                    _ => continue, // ignore other events
                }
                Some(Err(_)) => Event::Error,
                None => break, // the event stream has stopped and will not produce any more events
            },
        };
        if event_tx.send(event).is_err() {
            // the receiver has been dropped, so there's no point in continuing the loop
            break;
        }
    }
    cancellation_token.cancel();
}

impl<B: Backend> Deref for Tui<B> {
    type Target = ratatui::Terminal<B>;

    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl<B: Backend> DerefMut for Tui<B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.terminal
    }
}

impl<B: Backend> Drop for Tui<B> {
    fn drop(&mut self) {
        if let Err(e) = self.exit() {
            error!("Unable to exit the terminal: {e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn restore_terminal_twice_without_raw_mode() {
        assert!(restore_terminal().is_ok());
        assert!(restore_terminal().is_ok());
    }

    /// A `Tui` whose event task reads from a stream that never yields.
    fn test_tui() -> Tui<TestBackend> {
        let mut tui = Tui::with_backend(TestBackend::new(80, 24)).unwrap();
        tui.start_with(futures::stream::pending());
        tui
    }

    fn count_ticks(tui: &mut Tui<TestBackend>) -> usize {
        let mut ticks = 0;
        while let Ok(event) = tui.event_rx.try_recv() {
            if matches!(event, Event::Tick) {
                ticks += 1;
            }
        }
        ticks
    }

    #[tokio::test(start_paused = true)]
    async fn no_ticks_while_ticking_is_disabled() {
        let mut tui = test_tui();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(count_ticks(&mut tui), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn ten_ticks_per_second_while_ticking() {
        let mut tui = test_tui();
        tui.set_ticking(true);
        tokio::time::sleep(Duration::from_millis(1050)).await;
        let ticks = count_ticks(&mut tui);
        assert!((9..=11).contains(&ticks), "{ticks} ticks");

        // Turning it off stops the ticks.
        tui.set_ticking(false);
        tokio::task::yield_now().await;
        count_ticks(&mut tui);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(count_ticks(&mut tui), 0);
    }
}
