//! Render tests: drive `ui::render` through ratatui's `TestBackend` and assert on the
//! painted buffer. Adapts `herdr-reviewr`'s `tests/render.rs` harness.

use std::sync::atomic::AtomicBool;

use herdr_slackr::app::{App, Tab};
use herdr_slackr::model::{ConvKind, Message};
use herdr_slackr::rest::Rest;
use herdr_slackr::socket::SocketEvent;
use herdr_slackr::ui::{self, PaneState};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

fn dump(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

fn render_ready(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    let palette = herdr_slackr::theme::resolve(Some("catppuccin"));
    terminal.draw(|f| ui::render(f, &palette, &PaneState::Ready(app))).unwrap();
    dump(terminal.backend().buffer())
}

fn render_blocked(msg: &str) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    let palette = herdr_slackr::theme::resolve(Some("catppuccin"));
    terminal.draw(|f| ui::render(f, &palette, &PaneState::Blocked(msg))).unwrap();
    dump(terminal.backend().buffer())
}

fn msg(conv: &str, ts: &str, thread_ts: Option<&str>, author: &str, text: &str) -> Message {
    Message {
        conv: conv.into(),
        ts: ts.into(),
        thread_ts: thread_ts.map(str::to_owned),
        author: author.into(),
        text: text.into(),
        edited: false,
        reply_count: None,
    }
}

fn rest(cancelled: &AtomicBool) -> Rest<'_> {
    cancelled.store(true, std::sync::atomic::Ordering::Release);
    Rest { user_token: "xoxp-test", cancelled }
}

#[test]
fn feed_row_shows_channel_author_time_and_text() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.add_user("U1", "dan");
    app.apply(SocketEvent::Message(msg("C1", "1752300000.000100", None, "U1", "deploy done")));
    app.touch();

    let out = render_ready(&app);
    assert!(out.contains("#eng"), "{out}");
    assert!(out.contains("@dan"), "{out}");
    assert!(out.contains("06:00"), "{out}");
    assert!(out.contains("deploy done"), "{out}");
}

#[test]
fn thread_marker_renders_the_reply_count() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.add_user("U1", "dan");
    app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
    app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
    app.apply(SocketEvent::Message(msg("C1", "1.000003", Some("1.000001"), "U1", "reply two")));
    app.touch();

    let out = render_ready(&app);
    assert!(out.contains("\u{21b3} 2 replies"), "{out}");
}

#[test]
fn a_divider_line_renders_between_seen_and_unseen_rows() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "seen before touch")));
    app.touch();
    app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "arrived after touch")));

    let out = render_ready(&app);
    assert!(
        out.lines().any(|l| l.trim_end().len() > 10 && l.trim_end().chars().all(|c| c == '─')),
        "no full horizontal-rule divider line found:\n{out}"
    );
}

#[test]
fn mentions_tab_shows_read_unread_markers_and_the_tab_bar_count() {
    let mut app = App::empty("SELF");
    app.add_conversation("D1", "dan", ConvKind::Im);
    app.apply(SocketEvent::Message(msg("D1", "1.0", None, "U1", "ping")));
    app.apply(SocketEvent::Message(msg("D1", "2.0", None, "U1", "pong")));
    app.set_tab(Tab::Mentions);

    let out = render_ready(&app);
    assert!(out.contains("2 Mentions (2)"), "{out}");
    assert!(out.contains('\u{25cf}'), "unread rows carry the unread marker:\n{out}");

    let cancelled = AtomicBool::new(false);
    app.toggle_expand_or_read(&rest(&cancelled));
    let out = render_ready(&app);
    assert!(out.contains('\u{25cb}'), "the toggled row now shows read:\n{out}");
}

// ---- vertical scrolling (Fix 1): a tall row set in a short viewport ------------------------

/// A Feed-tab app with `n` distinct, ascending-`ts` messages, each rendering as `row {i}`.
fn tall_feed(n: usize) -> App {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    for i in 0..n {
        let ts = format!("{}.0", 1_000 + i);
        app.apply(SocketEvent::Message(msg("C1", &ts, None, "U1", &format!("row {i}"))));
    }
    app.touch();
    app
}

fn render_ready_sized(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let palette = herdr_slackr::theme::resolve(Some("catppuccin"));
    terminal.draw(|f| ui::render(f, &palette, &PaneState::Ready(app))).unwrap();
    dump(terminal.backend().buffer())
}

#[test]
fn cursor_at_the_top_keeps_the_first_row_visible_in_a_short_viewport() {
    let app = tall_feed(30); // cursor defaults to row 0

    let out = render_ready_sized(&app, 60, 10);
    assert!(out.contains("row 0"), "first row must be visible:\n{out}");
    assert!(!out.contains("row 29"), "the last row must be scrolled out of view:\n{out}");
}

#[test]
fn cursor_at_the_bottom_keeps_the_last_row_visible_in_a_short_viewport() {
    let mut app = tall_feed(30);
    app.move_cursor(1000); // clamps to the last row

    let out = render_ready_sized(&app, 60, 10);
    assert!(out.contains("row 29"), "last row must be visible with the cursor on it:\n{out}");
    assert!(!out.contains("row 0"), "the first row must be scrolled out of view:\n{out}");
}

#[test]
fn a_new_arrival_while_the_cursor_sits_at_the_bottom_stays_visible() {
    let mut app = tall_feed(30);
    app.move_cursor(1000); // sit at the bottom, the common at-the-bottom chat state

    app.apply(SocketEvent::Message(msg("C1", "5000.0", None, "U1", "brand new arrival")));

    let out = render_ready_sized(&app, 60, 10);
    assert!(
        out.contains("brand new arrival"),
        "a new arrival while pinned to the bottom must stay in view:\n{out}"
    );
}

#[test]
fn degraded_screen_names_the_tokens_and_the_tokens_toml_path() {
    let dir = tempfile::tempdir().unwrap();
    let err = herdr_slackr::tokens::resolve(dir.path(), |_| None).unwrap_err();

    let out = render_blocked(&err.to_string());
    assert!(out.contains("SLACK_APP_TOKEN"), "{out}");
    assert!(out.contains("tokens.toml"), "{out}");
}
