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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Pacing between post-reconnect catch-up batches (`App::catchup_tick`, a `POLL_BATCH`-request
/// budget each): fast enough that a typical subscription list is swept within a minute or two
/// of reconnecting, slow enough that the sweep's worst sustained rate (~8 requests / 15s ≈
/// 32/min) leaves real headroom under Slack's Tier-3 budget (~50/min) — deliberately
/// conservative because Slack rate limits pool per app + workspace, so a shared bot key means
/// this pane is never the only thing spending from it.
const CATCHUP_INTERVAL: Duration = Duration::from_secs(15);

/// How long a healthy-looking socket may stay completely silent before the event loop spends
/// one ordinary poll batch as a safety net (see the safety-poll block in `event_loop`). Five
/// minutes of zero events across every subscribed conversation is already suspicious in a busy
/// workspace and unremarkable in a quiet one — and the safety poll is cheap either way: at most
/// ~8 requests per 5 minutes, only while the silence lasts, nothing when events flow.
const SILENT_SOCKET_POLL: Duration = Duration::from_secs(300);

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

    // The terminal comes up *before* `App::build`: the backfill it runs can legitimately block
    // for a while (a rate-limited conversation sleeps up to 60s before its one retry — see
    // `app::backfill_one`), and without a frame drawn first the pane sits blank that whole
    // time, indistinguishable from a hang.
    let mut terminal = ratatui::init();
    let palette = theme::resolve(Some(&theme_name));
    let _ = terminal.draw(|f| {
        ui::render(
            f,
            &palette,
            &PaneState::Blocked(
                "connecting to slack — backfilling recent history\n\
                 (a rate-limited start can take up to a minute)",
            ),
        );
    });

    let result = match App::build(config, &tokens, &rest) {
        Ok(mut app) => {
            let (tx, rx) = mpsc::channel::<SocketEvent>();
            let worker_cancelled = cancelled.clone();
            let app_token = tokens.app.clone();
            let worker = thread::spawn(move || socket::run(app_token, tx, worker_cancelled));
            let result =
                event_loop(&mut terminal, &mut app, &rx, &rest, poll_fallback_secs, &theme_name);
            // Signal the worker and detach rather than join: `socket::run`'s read loop blocks
            // on a 30s read timeout with no state here that needs flushing, so joining it would
            // make `q` hang the terminal restore for up to 30s. The thread exits on its own
            // once `cancelled` is observed; the process must return control instantly.
            cancelled.store(true, Ordering::Release);
            drop(worker);
            result
        }
        Err(error) => blocked_loop(&mut terminal, &error),
    };
    ratatui::restore();
    result
}

fn plugin_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(PathBuf::from).unwrap_or_default()
}

/// Render the full-pane remedy screen until `q`. No socket, no REST, no crash — just the
/// actionable message (reviewr's degraded-state pattern).
fn run_blocked(msg: &str) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = blocked_loop(&mut terminal, msg);
    ratatui::restore();
    result
}

/// The remedy screen's draw-until-`q` loop, on an already-initialized terminal — shared
/// between `run_blocked` (config/token failures, which happen before any terminal exists) and
/// `run`'s `App::build` failure arm (which already drew the loading frame on its terminal).
fn blocked_loop(terminal: &mut DefaultTerminal, msg: &str) -> Result<()> {
    let palette = theme::resolve(None);
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
    let mut last_catchup: Option<Instant> = None;
    let poll_fallback = Duration::from_secs(poll_fallback_secs);
    let mut next_poll_gap = jittered(poll_fallback);
    let mut next_catchup_gap = jittered(CATCHUP_INTERVAL);
    let backoff_cycle = Duration::from_secs(socket::backoff_secs(0, |b| b));
    // Drawing is event-driven, not every-tick: a frame is only rebuilt after something that
    // can change it (a socket event, a poll/catch-up tick, a keypress, a terminal resize, or
    // the UTC day flipping under `format_ts`'s dated-timestamp rendering). An idle pane's
    // 250ms wakeups otherwise re-projected every stored message into rows each tick, burning
    // CPU that grew with the store for frames identical to the last one.
    let mut dirty = true;
    let mut drawn_day: Option<u64> = None;
    // Seeded to "now" rather than an already-elapsed instant: startup backfill just covered
    // everything, so the first safety poll is earned only after a genuinely silent interval.
    let mut last_live_event = Instant::now();
    // Set once a safety poll finds messages the socket should have delivered; cleared by the
    // next live event. While set, safety polling runs at fallback cadence instead of the slow
    // five-minute diagnostic cadence — see the safety-poll block below.
    let mut socket_lossy = false;

    loop {
        while let Ok(ev) = rx.try_recv() {
            let went_down = matches!(ev, SocketEvent::Down(_));
            let reconnected = matches!(ev, SocketEvent::Connected);
            let is_live_message = matches!(
                ev,
                SocketEvent::Message(_) | SocketEvent::Changed(_) | SocketEvent::Deleted { .. }
            );
            app.apply(ev);
            dirty = true;
            if is_live_message {
                last_live_event = Instant::now();
                if socket_lossy {
                    // The socket proved itself again — drop back to the slow 5-minute
                    // safety cadence rather than keeping fallback-speed polling forever.
                    socket_lossy = false;
                    crate::logln!("safety poll: live events resumed — degraded polling ends");
                }
            }
            if went_down {
                down_since.get_or_insert_with(Instant::now);
            }
            if reconnected {
                down_since = None;
            }
        }

        // Silent-socket safety poll: a socket that connects but never delivers (the signature
        // of a Slack app missing its `message.*` event subscriptions — plausible on a shared
        // corporate app someone else configured) would otherwise freeze the feed forever,
        // because a "healthy" socket suppresses all polling. After SILENT_SOCKET_POLL with no
        // live event, spend one ordinary poll batch as a safety net; if it finds messages the
        // socket should have delivered, say so — that one status line is the difference
        // between "quiet afternoon" and "misconfigured app" from the user's chair.
        //
        // Once a safety poll has *proved* the socket lossy, one batch per five minutes is a
        // miserable cadence to live behind (a given conversation would refresh every
        // ~20 minutes on a typical subscription list) — so `socket_lossy` escalates the
        // cadence to the ordinary `poll_fallback_secs` rhythm, exactly what a socket-down
        // outage costs, until a live event actually arrives and proves the socket healed.
        if !app.polling
            && last_live_event.elapsed() >= SILENT_SOCKET_POLL
            && last_poll_tick.elapsed()
                >= if socket_lossy { next_poll_gap } else { SILENT_SOCKET_POLL }
        {
            if !socket_lossy {
                crate::logln!(
                    "safety poll: socket connected but no live events for {}s",
                    SILENT_SOCKET_POLL.as_secs()
                );
            }
            let before = app.arrival_count();
            app.poll_tick(rest);
            if app.arrival_count() > before {
                app.status = "live events silent but polling found new messages — check the \
                              Slack app's event subscriptions (README §Slack app setup)"
                    .to_string();
                crate::logln!(
                    "safety poll: found {} messages the socket never delivered",
                    app.arrival_count() - before
                );
                if !socket_lossy {
                    socket_lossy = true;
                    crate::logln!(
                        "safety poll: socket confirmed lossy — polling every ~{}s until live \
                         events resume",
                        poll_fallback_secs
                    );
                }
            }
            last_poll_tick = Instant::now();
            next_poll_gap = jittered(poll_fallback);
            dirty = true;
        }

        // Both recurring request cadences below re-jitter their next interval after every
        // firing (see `jittered`): panes sharing one app key all enter polling mode anchored
        // to the same Slack outage, and fixed intervals would keep their request batches in
        // lockstep against the shared rate-limit pool indefinitely.
        if app.polling
            && down_since.is_some_and(|since| since.elapsed() >= backoff_cycle)
            && last_poll_tick.elapsed() >= next_poll_gap
        {
            app.poll_tick(rest);
            last_poll_tick = Instant::now();
            next_poll_gap = jittered(poll_fallback);
            dirty = true;
        }

        // Post-reconnect catch-up (spec §Polling fallback): the socket never redelivers events
        // missed while it was down, so once it is healthy again the armed sweep re-fetches every
        // subscribed conversation from its watermark, one paced batch at a time. Only while the
        // socket is up — during a renewed outage the ordinary poll fallback above covers
        // freshness, and the next `Connected` re-arms the sweep in full anyway.
        if !app.polling
            && app.catchup_due()
            && last_catchup.is_none_or(|at: Instant| at.elapsed() >= next_catchup_gap)
        {
            app.catchup_tick(rest);
            last_catchup = Some(Instant::now());
            next_catchup_gap = jittered(CATCHUP_INTERVAL);
            dirty = true;
        }

        let today = utc_day(SystemTime::now());
        if drawn_day != Some(today) {
            dirty = true;
        }

        if dirty {
            let unread = app.unread_mentions();
            if unread != last_unread {
                let _ = write!(io::stdout(), "{}", ui::nav_title(unread));
                let _ = io::stdout().flush();
                last_unread = unread;
            }

            terminal.draw(|f| {
                // Threaded into `App` once per draw (rather than measured only on a page-move
                // key) so `page_move`'s caller below always has the pane's current on-screen
                // height, including right after a terminal resize — see
                // `App::set_viewport_rows`'s doc and `ui::body_rows` for the same chrome-row
                // math `render_ready`'s layout uses.
                app.set_viewport_rows(ui::body_rows(f.area().height));
                ui::render(f, &palette, &PaneState::Ready(app));
            })?;
            drawn_day = Some(today);
            dirty = false;
        }

        if !event::poll(TICK)? {
            continue;
        }
        // Read as a match, not a Key-only let-chain: a `Resize` must mark the frame dirty too
        // (drawing is no longer unconditional, so a resize with no other traffic would
        // otherwise leave the old layout on screen until the next unrelated event).
        let k = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => k,
            Event::Resize(_, _) => {
                dirty = true;
                continue;
            }
            _ => continue,
        };
        app.touch();
        dirty = true;
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
            KeyCode::Char('f') => app.toggle_focus(),
            KeyCode::Char('o') => {
                if let Some(url) = app.permalink_of_selected(rest)
                    && let Err(error) = browser::open(&url)
                {
                    app.status = format!("open failed: {error}");
                }
            }
            KeyCode::Char('r') => {
                // Manual refresh: spend one poll batch right now for immediacy, and arm the
                // paced catch-up sweep so the rest of the subscription list follows under the
                // usual request budget (a lone poll_tick only covered the next 8-request
                // round-robin slice, which on a large subscription list refreshed almost
                // nothing of what the user was actually asking for).
                app.request_refresh();
                app.poll_tick(rest);
            }
            _ => {}
        }
    }
}

/// `base` with `±25%` jitter applied — the same spread (and the same cheap clock-derived
/// entropy) as `socket::run`'s reconnect schedule, here de-synchronizing the poll-fallback and
/// catch-up cadences across panes: a Slack outage flips *every* pane on a shared app key into
/// polling mode anchored to the same outage moment, and without jitter they then fire their
/// request batches in lockstep against one shared rate-limit pool. With ±25% per cycle, the
/// cohort spreads out within a few cycles instead of thundering together indefinitely.
fn jittered(base: Duration) -> Duration {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let pct = 75 + u64::from(nanos % 51); // 75..=125
    base.saturating_mul(u32::try_from(pct).expect("pct is at most 125")) / 100
}

/// Whole UTC days since the epoch, for the event loop's redraw-on-day-flip check: row
/// timestamps render dated once their UTC calendar day is no longer "today" (`app::format_ts`),
/// so a frame drawn before UTC midnight goes stale at midnight even with no state change —
/// the only time-driven reason to redraw now that drawing is otherwise event-driven.
fn utc_day(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) / 86_400
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
    use super::{theme_warning, utc_day};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn jittered_stays_within_the_twenty_five_percent_band() {
        // The exact value is entropy-driven; the contract is the band. Sampling repeatedly
        // keeps a bad constant (e.g. an off-by-percent bound) from passing by luck.
        let base = Duration::from_secs(30);
        for _ in 0..100 {
            let j = super::jittered(base);
            assert!(j >= Duration::from_millis(22_500), "{j:?} below 75% of base");
            assert!(j <= Duration::from_millis(37_500), "{j:?} above 125% of base");
        }
    }

    #[test]
    fn utc_day_counts_whole_days_since_the_epoch() {
        assert_eq!(utc_day(UNIX_EPOCH), 0);
        assert_eq!(utc_day(UNIX_EPOCH + Duration::from_secs(86_399)), 0);
        assert_eq!(utc_day(UNIX_EPOCH + Duration::from_secs(86_400)), 1);
    }

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
