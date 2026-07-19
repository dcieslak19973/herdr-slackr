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
use ratatui::style::Color;

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

/// The `(x, y)` of `text`'s first character in `buffer`, scanning row by row — used so a style
/// assertion targets the actual on-screen position of a rendered segment rather than a
/// hand-computed offset that would silently drift out of sync with `row_line`'s layout.
fn find_text_position(buffer: &Buffer, text: &str) -> Option<(u16, u16)> {
    let area = buffer.area;
    let wanted: Vec<char> = text.chars().collect();
    for y in 0..area.height {
        for x in 0..area.width {
            if x + wanted.len() as u16 > area.width {
                continue;
            }
            let matches = wanted.iter().enumerate().all(|(i, c)| {
                buffer.cell((x + i as u16, y)).is_some_and(|cell| cell.symbol() == c.to_string())
            });
            if matches {
                return Some((x, y));
            }
        }
    }
    None
}

fn fg_at(buffer: &Buffer, text: &str) -> Color {
    let (x, y) = find_text_position(buffer, text).unwrap_or_else(|| panic!("{text:?} not found"));
    buffer.cell((x, y)).unwrap().fg
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
fn a_collapsed_threads_marker_carries_the_count_and_latest_reply_snippet() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.add_user("U1", "dan");
    app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
    app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
    app.touch();

    let out = render_ready(&app);
    // One enriched marker row, no scattered per-reply activity rows.
    assert!(out.contains("\u{21b3} 1 reply · @dan: reply one"), "{out}");
    assert!(!out.contains("replied:"), "{out}");
}

#[test]
fn an_expanded_threads_reply_shows_no_activity_row() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.add_user("U1", "dan");
    app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
    app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
    app.touch();
    // cursor defaults to the root's own Message row; Enter on it expands the thread (spec §5).
    let cancelled = AtomicBool::new(false);
    app.toggle_expand_or_read(&rest(&cancelled));

    let out = render_ready(&app);
    assert!(!out.contains("replied:"), "{out}");
    assert!(out.contains("reply one"), "{out}");
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
    // With no tokens at all, the *user* token is what fails resolution — the app token's
    // absence is a valid state now (poll-only mode), never an error.
    let dir = tempfile::tempdir().unwrap();
    let err = herdr_slackr::tokens::resolve(dir.path(), |_| None).unwrap_err();

    let out = render_blocked(&err.to_string());
    assert!(out.contains("SLACK_USER_TOKEN"), "{out}");
    assert!(out.contains("tokens.toml"), "{out}");
}

// ---- row colors (spec §4): conv label blue-family, author green, time/markers muted -------

#[test]
fn feed_row_segments_render_in_their_spec_colors() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.add_user("U1", "dan");
    app.apply(SocketEvent::Message(msg("C1", "1752300000.000100", None, "U1", "deploy done")));
    app.touch();

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    let palette = herdr_slackr::theme::resolve(Some("catppuccin"));
    terminal.draw(|f| ui::render(f, &palette, &PaneState::Ready(&app))).unwrap();
    let buffer = terminal.backend().buffer();

    assert_eq!(fg_at(buffer, "#eng"), palette.lavender, "conv label is the blue-family accent");
    assert_eq!(fg_at(buffer, "@dan"), palette.green, "author is green");
    assert_eq!(fg_at(buffer, "06:00"), palette.overlay1, "time is the muted overlay tone");
    assert_eq!(fg_at(buffer, "deploy done"), palette.text, "message text is the default fg");
}

#[test]
fn thread_marker_text_renders_in_the_muted_overlay_tone() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.add_user("U1", "dan");
    app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
    app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
    app.apply(SocketEvent::Message(msg("C1", "1.000003", Some("1.000001"), "U1", "reply two")));
    app.touch();

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    let palette = herdr_slackr::theme::resolve(Some("catppuccin"));
    terminal.draw(|f| ui::render(f, &palette, &PaneState::Ready(&app))).unwrap();
    let buffer = terminal.backend().buffer();

    assert_eq!(fg_at(buffer, "\u{21b3} 2 replies"), palette.overlay1);
}

// ---- Threads view (spec §3) -----------------------------------------------------------------

#[test]
fn threads_view_shows_only_threads_with_replies_nested_beneath_their_root() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.add_user("U1", "dan");
    app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "not a thread")));
    app.apply(SocketEvent::Message(Message {
        reply_count: Some(1),
        ..msg("C1", "2.0", None, "U1", "thread root")
    }));
    app.apply(SocketEvent::Message(msg("C1", "2.1", Some("2.0"), "U1", "a reply")));
    app.touch();
    app.toggle_view();

    let out = render_ready(&app);
    assert!(out.contains("thread root"), "{out}");
    assert!(out.contains("a reply"), "{out}");
    assert!(!out.contains("not a thread"), "non-threaded messages must be excluded:\n{out}");
}

#[test]
fn tab_bar_names_the_active_feed_view_mode() {
    let mut app = App::empty("SELF");
    let out = render_ready(&app);
    assert!(out.contains("1 Feed"), "{out}");

    app.toggle_view();
    let out = render_ready(&app);
    assert!(out.contains("1 Threads"), "{out}");
}

// ---- Focus view (Task 3, spec §3) -----------------------------------------------------------

#[test]
fn tab_bar_names_focus_when_the_focus_view_is_active() {
    let mut app = App::empty("SELF");
    app.toggle_focus();
    let out = render_ready(&app);
    assert!(out.contains("1 Focus"), "{out}");
}

#[test]
fn focus_view_shows_only_qualifying_live_messages_like_the_timeline() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.add_user("U1", "dan");
    app.add_focus_keyword("urgent");
    app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "an urgent update")));
    app.apply(SocketEvent::Message(msg("C1", "2.0", None, "U1", "a plain message")));
    app.touch();
    app.toggle_focus();

    let out = render_ready(&app);
    assert!(out.contains("an urgent update"), "{out}");
    assert!(
        !out.contains("a plain message"),
        "a non-matching message must not show in Focus:\n{out}"
    );
    // Same row shape as the Timeline: conv label, author, time all present (spec: "reuses the
    // timeline row layout").
    assert!(out.contains("#eng"), "{out}");
    assert!(out.contains("@dan"), "{out}");
}

#[test]
fn focus_view_includes_an_allow_listed_dm_message_with_no_keyword_needed() {
    let mut app = App::empty("SELF");
    app.add_conversation("D1", "alice", ConvKind::Im);
    app.allow_focus_dm("D1");
    app.apply(SocketEvent::Message(msg("D1", "1.0", None, "U1", "just checking in")));
    app.touch();
    app.toggle_focus();

    let out = render_ready(&app);
    assert!(out.contains("just checking in"), "{out}");
}

#[test]
fn focus_view_is_empty_when_nothing_qualifies_yet() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "nothing matches")));
    app.touch();
    app.toggle_focus();

    let out = render_ready(&app);
    assert!(!out.contains("nothing matches"), "{out}");
}

#[test]
fn f_key_toggle_is_mutually_exclusive_with_threads() {
    let mut app = App::empty("SELF");
    app.toggle_view(); // t: Timeline -> Threads
    let out = render_ready(&app);
    assert!(out.contains("1 Threads"), "{out}");

    app.toggle_focus(); // f from Threads lands on Focus, not Timeline
    let out = render_ready(&app);
    assert!(out.contains("1 Focus"), "{out}");

    app.toggle_view(); // t from Focus lands on Threads, not Timeline
    let out = render_ready(&app);
    assert!(out.contains("1 Threads"), "{out}");
}

// ---- status-line `f` hint on the Feed tab ----------------------------------------------------

#[test]
fn feed_tab_status_hints_the_f_toggle_focus_key() {
    let app = App::empty("SELF");
    let out = render_ready(&app);
    assert!(out.to_lowercase().contains("f: focus"), "{out}");
}

#[test]
fn mentions_tab_status_has_no_f_hint() {
    let mut app = App::empty("SELF");
    app.set_tab(Tab::Mentions);
    let out = render_ready(&app);
    assert!(!out.to_lowercase().contains("f: focus"), "{out}");
}

#[test]
fn t_key_toggle_flips_the_feed_view_and_the_timeline_survives_a_flip_back() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "a plain message")));
    app.touch();

    app.toggle_view();
    let out = render_ready(&app);
    assert!(
        !out.contains("a plain message"),
        "the Threads view excludes non-threaded messages:\n{out}"
    );

    app.toggle_view();
    let out = render_ready(&app);
    assert!(
        out.contains("a plain message"),
        "the Timeline is unchanged after flipping back:\n{out}"
    );
}

// ---- status-line `t` hint on the Feed tab ----------------------------------------------------

#[test]
fn feed_tab_status_hints_the_t_toggle_key() {
    let app = App::empty("SELF");
    let out = render_ready(&app);
    assert!(out.contains('t'), "{out}"); // loose smoke check before the exact-text assertion below
    assert!(out.to_lowercase().contains("t: threads"), "{out}");
}

#[test]
fn mentions_tab_status_has_no_t_hint() {
    let mut app = App::empty("SELF");
    app.set_tab(Tab::Mentions);
    let out = render_ready(&app);
    assert!(!out.to_lowercase().contains("t: threads"), "{out}");
}

// ---- poll-only footer marker: a permanent mode is a compact marker, not a status sentence ----

#[test]
fn poll_only_mode_renders_as_a_compact_marker_leaving_room_for_the_hints() {
    let mut app = App::empty("SELF");
    app.poll_only = true;
    app.polling = true;
    let out = render_ready(&app);
    assert!(out.contains("poll-only"), "{out}");
    // The marker replaces (never doubles with) the generic `polling` marker...
    assert!(!out.contains("poll-only · polling"), "{out}");
    // ...and the whole footer still fits: the last hint survives on a 100-column frame.
    assert!(out.to_lowercase().contains("g/g: top/bottom"), "{out}");
}

// ---- thread expand/collapse completion feedback (Task 2, spec §5) ---------------------------

#[test]
fn collapsing_a_thread_shows_a_collapsed_status_line() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
    app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
    app.touch();
    let cancelled = AtomicBool::new(false);
    app.toggle_expand_or_read(&rest(&cancelled)); // expand (cursor defaults to the root row)
    app.cursor = 0;
    app.toggle_expand_or_read(&rest(&cancelled)); // collapse back

    let out = render_ready(&app);
    assert!(out.contains("thread collapsed"), "{out}");
}

// ---- nav keys + new-arrivals indicator (Task 1, spec §2-§4) --------------------------------

// ---- context-aware "enter expand/collapse thread" footer hint (Task 2, spec §5) -------------

#[test]
fn footer_hints_enter_to_expand_when_the_cursor_is_on_a_thread_roots_message_row() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
    app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
    app.touch(); // cursor defaults to row 0: the root's own Message row.

    let out = render_ready(&app);
    assert!(out.to_lowercase().contains("enter expand/collapse thread"), "{out}");
}

#[test]
fn footer_hints_enter_to_expand_when_the_cursor_is_on_the_marker_row() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
    app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
    app.touch();
    app.move_cursor(1); // the ThreadMarker row

    let out = render_ready(&app);
    assert!(out.to_lowercase().contains("enter expand/collapse thread"), "{out}");
}

#[test]
fn footer_hints_enter_to_collapse_when_the_cursor_is_on_an_expanded_reply_rail_row() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
    app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
    app.touch();
    // Expand via Enter on the root, then select the reply's rail row.
    let cancelled = AtomicBool::new(false);
    app.toggle_expand_or_read(&rest(&cancelled));
    app.move_cursor(1);
    // The cancelled fetch leaves a "replies failed" status that would push the hint past the
    // test terminal's 100-column clip; only the hint is under test here.
    app.status.clear();

    let out = render_ready(&app);
    assert!(out.to_lowercase().contains("enter expand/collapse thread"), "{out}");
}

#[test]
fn expanded_replies_render_a_muted_connector_rail_in_place_of_the_channel_label() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.add_user("U1", "dan");
    app.apply(SocketEvent::Message(msg("C1", "1.000001", None, "U1", "root")));
    app.apply(SocketEvent::Message(msg("C1", "1.000002", Some("1.000001"), "U1", "reply one")));
    app.apply(SocketEvent::Message(msg("C1", "1.000003", Some("1.000001"), "U1", "reply two")));
    app.touch();
    let cancelled = AtomicBool::new(false);
    app.toggle_expand_or_read(&rest(&cancelled)); // expand via the root row

    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    let palette = herdr_slackr::theme::resolve(Some("catppuccin"));
    terminal.draw(|f| ui::render(f, &palette, &PaneState::Ready(&app))).unwrap();
    let buffer = terminal.backend().buffer();

    let out = render_ready(&app);
    assert!(out.contains("\u{251c}\u{2500}"), "mid-thread reply shows the ├─ rail:\n{out}");
    assert!(out.contains("\u{2514}\u{2500}"), "the last reply closes with the └─ rail:\n{out}");
    assert_eq!(
        fg_at(buffer, "\u{251c}\u{2500}"),
        palette.overlay1,
        "the rail is muted, not channel-colored"
    );
}

#[test]
fn footer_has_no_thread_hint_when_the_cursor_is_on_a_plain_message() {
    let mut app = App::empty("SELF");
    app.add_conversation("C1", "eng", ConvKind::Channel);
    app.apply(SocketEvent::Message(msg("C1", "1.0", None, "U1", "just chatting")));
    app.touch();

    let out = render_ready(&app);
    assert!(!out.to_lowercase().contains("enter expand/collapse thread"), "{out}");
}

#[test]
fn status_hints_the_top_bottom_jump_keys_on_every_tab() {
    let mut app = App::empty("SELF");
    let out = render_ready(&app);
    assert!(out.to_lowercase().contains("g/g: top/bottom"), "{out}");

    app.set_tab(Tab::Mentions);
    let out = render_ready(&app);
    assert!(out.to_lowercase().contains("g/g: top/bottom"), "{out}");
}

#[test]
fn jump_newest_keeps_the_last_row_visible_in_a_short_viewport() {
    let mut app = tall_feed(30);
    app.jump_first(); // start scrolled away from the bottom
    app.jump_newest();

    let out = render_ready_sized(&app, 60, 10);
    assert!(out.contains("row 29"), "G/End must land on the last row:\n{out}");
    assert!(!out.contains("row 0"), "the first row scrolls out of view:\n{out}");
}

#[test]
fn jump_first_keeps_the_first_row_visible_in_a_short_viewport() {
    let mut app = tall_feed(30);
    app.jump_newest(); // start at the bottom
    app.jump_first();

    let out = render_ready_sized(&app, 60, 10);
    assert!(out.contains("row 0"), "g/Home must land on the first row:\n{out}");
    assert!(!out.contains("row 29"), "the last row scrolls out of view:\n{out}");
}

#[test]
fn the_new_arrivals_indicator_is_hidden_with_nothing_pending() {
    let mut app = tall_feed(30);
    // `tall_feed` builds via plain `apply` calls, whose cursor defaults to row 0 (see the
    // top-viewport test above) rather than `build`'s explicit at-the-bottom snap — establish
    // that baseline explicitly here so this test starts from the common "caught up" state.
    app.jump_newest();
    let out = render_ready_sized(&app, 60, 10);
    assert!(!out.contains("new"), "no overlay expected when pending_new is 0:\n{out}");
}

#[test]
fn the_new_arrivals_indicator_shows_the_pending_count_once_scrolled_up() {
    let mut app = tall_feed(30);
    app.jump_newest(); // establish the at-the-bottom baseline (see the test above)
    app.jump_first(); // scroll away from the bottom

    app.apply(SocketEvent::Message(msg("C1", "5000.0", None, "U1", "an arrival off-screen")));

    let out = render_ready_sized(&app, 60, 10);
    assert!(out.contains("\u{2193} 1 new"), "expected the indicator with count 1:\n{out}");
}
