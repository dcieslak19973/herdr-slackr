//! herdr-slackr — a herdr-native Slack feed pane.
//!
//! [`run`] is the entry point: resolve config/tokens, build the initial [`app::App`], spawn
//! the socket worker, then drive the terminal event loop (crossterm, 250ms tick) draining the
//! worker's channel into `App::apply`. See
//! `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md` §Architecture and §The pane.

pub mod app;
pub mod browser;
pub mod cli;
pub mod config;
pub mod entities;
#[macro_use]
pub mod log;
pub mod model;
pub mod proc;
pub mod rest;
pub mod socket;
pub mod theme;
pub mod tokens;
pub mod ui;
pub mod users_cache;

use std::io::{self, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::app::{App, Tab};
use crate::rest::Rest;
use crate::socket::SocketEvent;
use crate::ui::PaneState;

/// The event loop's tick: how often crossterm's input poll wakes up to drain the socket
/// channel and re-check the polling-fallback clock even with no keypress pending.
const TICK: Duration = Duration::from_millis(250);

/// Entry point: resolve config/tokens, build the pane, run the loop, restore the terminal.
/// A config or token failure (or `App::build`'s own REST failure) never crashes — it renders
/// the full-pane remedy screen instead (spec §Error handling).
pub fn run() -> Result<()> {
    log::init();
    let dir = plugin_dir();

    let config = match config::plugin_config() {
        Ok(config) => config,
        Err(error) => return run_blocked(&error.to_string()),
    };
    let tokens = match tokens::resolve(&dir, |name| std::env::var(name).ok()) {
        Ok(tokens) => tokens,
        Err(error) => return run_blocked(&error.0),
    };
    let theme_name = config.theme().to_string();
    let poll_fallback_secs = config.poll_fallback_secs();

    let cancelled = Arc::new(AtomicBool::new(false));
    let rest = Rest { user_token: &tokens.user, cancelled: &cancelled };
    let mut app = match App::build(config, &tokens, &rest) {
        Ok(app) => app,
        Err(error) => return run_blocked(&error),
    };

    let (tx, rx) = mpsc::channel::<SocketEvent>();
    let worker_cancelled = cancelled.clone();
    let app_token = tokens.app.clone();
    let worker = thread::spawn(move || socket::run(app_token, tx, worker_cancelled));

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &rx, &rest, poll_fallback_secs, &theme_name);
    ratatui::restore();

    // Signal the worker and detach rather than join: `socket::run`'s read loop blocks on a
    // 30s read timeout with no state here that needs flushing, so joining it would make `q`
    // hang the terminal restore for up to 30s. The thread exits on its own once `cancelled`
    // is observed; the process must return control instantly.
    cancelled.store(true, Ordering::Release);
    drop(worker);
    result
}

fn plugin_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(PathBuf::from).unwrap_or_default()
}

/// Render the full-pane remedy screen until `q`. No socket, no REST, no crash — just the
/// actionable message (reviewr's degraded-state pattern).
fn run_blocked(msg: &str) -> Result<()> {
    let mut terminal = ratatui::init();
    let palette = theme::resolve(None);
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| ui::render(f, &palette, &PaneState::Blocked(msg)))?;
            if event::poll(TICK)?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
                && k.code == KeyCode::Char('q')
            {
                return Ok(());
            }
        }
    })();
    ratatui::restore();
    result
}

/// Draw, drain the socket channel, and dispatch keys, until `q`.
///
/// Fallback semantics (spec §Polling fallback): `App::apply` already flips `polling` the
/// instant a `Down` event arrives — that alone would poll from the very first hiccup, racing
/// the worker's own fast reconnect. So this loop tracks how long the *current* down streak has
/// lasted and only starts calling `poll_tick` once it has outlived one full backoff cycle
/// (`socket::backoff_secs(0, ...)`, the worker's first retry interval), thereafter every
/// `poll_fallback_secs` until a `Connected` event clears the streak.
fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mpsc::Receiver<SocketEvent>,
    rest: &Rest,
    poll_fallback_secs: u64,
    theme_name: &str,
) -> Result<()> {
    let palette = theme::resolve(Some(theme_name));
    if let Some(warning) = theme_warning(theme_name) {
        app.status = warning;
    }
    let mut last_unread = app.unread_mentions();
    let mut down_since: Option<Instant> = None;
    let mut last_poll_tick = Instant::now();
    let poll_fallback = Duration::from_secs(poll_fallback_secs);
    let backoff_cycle = Duration::from_secs(socket::backoff_secs(0, |b| b));

    loop {
        while let Ok(ev) = rx.try_recv() {
            let went_down = matches!(ev, SocketEvent::Down(_));
            let reconnected = matches!(ev, SocketEvent::Connected);
            app.apply(ev);
            if went_down {
                down_since.get_or_insert_with(Instant::now);
            }
            if reconnected {
                down_since = None;
            }
        }

        if app.polling
            && down_since.is_some_and(|since| since.elapsed() >= backoff_cycle)
            && last_poll_tick.elapsed() >= poll_fallback
        {
            app.poll_tick(rest);
            last_poll_tick = Instant::now();
        }

        let unread = app.unread_mentions();
        if unread != last_unread {
            let _ = write!(io::stdout(), "{}", ui::nav_title(unread));
            let _ = io::stdout().flush();
            last_unread = unread;
        }

        terminal.draw(|f| {
            // Threaded into `App` once per draw (rather than measured only on a page-move key)
            // so `page_move`'s caller below always has the pane's current on-screen height,
            // including right after a terminal resize — see `App::set_viewport_rows`'s doc and
            // `ui::body_rows` for the same chrome-row math `render_ready`'s layout uses.
            app.set_viewport_rows(ui::body_rows(f.area().height));
            ui::render(f, &palette, &PaneState::Ready(app));
        })?;

        if event::poll(TICK)?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            app.touch();
            match k.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('1') => app.set_tab(Tab::Feed),
                KeyCode::Char('2') => app.set_tab(Tab::Mentions),
                KeyCode::Tab => {
                    let next = if app.tab == Tab::Feed { Tab::Mentions } else { Tab::Feed };
                    app.set_tab(next);
                }
                KeyCode::Char('j') | KeyCode::Down => app.move_cursor(1),
                KeyCode::Char('k') | KeyCode::Up => app.move_cursor(-1),
                KeyCode::Char('G') | KeyCode::End => app.jump_newest(),
                KeyCode::Char('g') | KeyCode::Home => app.jump_first(),
                KeyCode::PageDown => app.page_move(app.viewport_rows() as isize),
                KeyCode::PageUp => app.page_move(-(app.viewport_rows() as isize)),
                KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.page_move((app.viewport_rows() / 2) as isize);
                }
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.page_move(-((app.viewport_rows() / 2) as isize));
                }
                KeyCode::Enter => app.toggle_expand_or_read(rest),
                KeyCode::Char('t') => app.toggle_view(),
                KeyCode::Char('o') => {
                    if let Some(url) = app.permalink_of_selected(rest)
                        && let Err(error) = browser::open(&url)
                    {
                        app.status = format!("open failed: {error}");
                    }
                }
                KeyCode::Char('r') => app.poll_tick(rest),
                _ => {}
            }
        }
    }
}

/// The one-line status to show when `theme_name` doesn't resolve to a known palette, or
/// `None` when it does. Pulled out of `event_loop` so the fallback message is unit-testable
/// without a terminal.
fn theme_warning(theme_name: &str) -> Option<String> {
    if theme::is_known(theme_name) {
        None
    } else {
        Some(format!("unknown theme '{theme_name}' — using {}", theme::DEFAULT))
    }
}

#[cfg(test)]
mod tests {
    use super::theme_warning;

    #[test]
    fn known_theme_has_no_warning() {
        assert_eq!(theme_warning("catppuccin"), None);
    }

    #[test]
    fn unknown_theme_warns_and_names_the_fallback() {
        let warning = theme_warning("catppuccin-mocha").expect("should warn");
        assert!(warning.contains("catppuccin-mocha"), "{warning}");
        assert!(warning.contains("catppuccin"), "{warning}");
    }
}
