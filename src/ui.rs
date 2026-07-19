//! Rendering the Feed/Mentions pane: tab bar, row list, status line. See
//! `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md` §The pane.
//!
//! Two states: a normal frame over an [`App`] ([`PaneState::Ready`]), or a full-pane remedy
//! screen naming what's missing ([`PaneState::Blocked`]) — missing/invalid tokens or config
//! never crash the pane, they show a fixable message instead (reviewr's degraded-state
//! pattern). Rendering reads `App` only; all state changes live in `app.rs`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, FeedView, Row, RowKind, Tab};
use crate::theme::Palette;

/// What [`render`] draws this frame.
#[derive(Debug)]
pub enum PaneState<'a> {
    /// Tokens/config failed to resolve, or `App::build` itself failed; `0` names the fix
    /// (env vars, the `tokens.toml` path, or the build error verbatim).
    Blocked(&'a str),
    /// The working pane.
    Ready(&'a App),
}

/// Draw one frame: the blocked remedy screen, or the tab bar + row list + status line.
pub fn render(frame: &mut Frame, palette: &Palette, state: &PaneState) {
    match state {
        PaneState::Blocked(msg) => render_blocked(frame, palette, msg),
        PaneState::Ready(app) => render_ready(frame, palette, app),
    }
}

/// The full-pane remedy screen: just the message, word-wrapped, no chrome — so it stays
/// legible at any width and can never be mistaken for a working pane.
fn render_blocked(frame: &mut Frame, palette: &Palette, msg: &str) {
    let area = frame.area();
    let p = Paragraph::new(msg.to_string())
        .style(Style::default().fg(palette.text))
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

fn render_ready(frame: &mut Frame, palette: &Palette, app: &App) {
    let area = frame.area();
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    render_tab_bar(frame, palette, app, rows[0]);
    render_body(frame, palette, app, rows[1]);
    render_status(frame, palette, app, rows[2]);
}

/// `1 Feed  2 Mentions (n)`, the active tab underlined and bright. The Feed label itself names
/// the active `FeedView` (`1 Feed` in the Timeline, `1 Threads` once toggled — spec §3's minimal
/// mode marker) rather than adding a separate indicator, since the Feed tab is the only place
/// the mode is ever relevant.
fn render_tab_bar(frame: &mut Frame, palette: &Palette, app: &App, area: Rect) {
    let n = app.unread_mentions();
    let feed_label = match app.view {
        FeedView::Timeline => "1 Feed",
        FeedView::Threads => "1 Threads",
        FeedView::Focus => "1 Focus",
    };
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(feed_label, tab_style(palette, app.tab == Tab::Feed)),
        Span::raw("  "),
        Span::styled(format!("2 Mentions ({n})"), tab_style(palette, app.tab == Tab::Mentions)),
    ]);
    frame.render_widget(Paragraph::new(line).style(Style::default().bg(palette.surface0)), area);
}

fn tab_style(palette: &Palette, active: bool) -> Style {
    if active {
        Style::default().fg(palette.lavender).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default().fg(palette.subtext0)
    }
}

/// The active tab's rows, the selected one filled with the cursor color, scrolled so the
/// cursor row always stays on-screen (see `scroll_offset`) — without this, a tall row set
/// simply painted every row from the top, leaving live arrivals and a cursor walked past the
/// last visible line invisible below the viewport.
fn render_body(frame: &mut Frame, palette: &Palette, app: &App, area: Rect) {
    let rows = match app.tab {
        Tab::Feed => match app.view {
            FeedView::Timeline => app.feed_rows(),
            FeedView::Threads => app.thread_rows(),
            FeedView::Focus => app.focus_rows(),
        },
        Tab::Mentions => app.mention_rows(),
    };
    let width = area.width as usize;
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| row_line(palette, row, width, i == app.cursor))
        .collect();
    let offset = scroll_offset(lines.len(), app.cursor, area.height as usize);
    #[allow(clippy::cast_possible_truncation)]
    let offset = offset as u16;
    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), area);
    render_new_arrivals_indicator(frame, palette, area, app.pending_new());
}

/// The bottom-edge `↓ n new` overlay (spec §3), drawn over the body's last on-screen row,
/// right-aligned, in the same muted `overlay1` accent `row_line` uses for markers/dividers, with
/// the same `surface0` fill the tab bar/status line use so it stays legible over whatever row
/// happens to sit last. Drawn after the row `Paragraph` so it paints on top; a no-op when
/// nothing is pending or the body has no rows to overlay onto.
fn render_new_arrivals_indicator(frame: &mut Frame, palette: &Palette, area: Rect, pending: usize) {
    if pending == 0 || area.height == 0 || area.width == 0 {
        return;
    }
    let text = format!("\u{2193} {pending} new");
    #[allow(clippy::cast_possible_truncation)]
    let width = (text.chars().count() as u16).min(area.width);
    let overlay =
        Rect { x: area.x + area.width - width, y: area.y + area.height - 1, width, height: 1 };
    let p = Paragraph::new(text).style(Style::default().fg(palette.overlay1).bg(palette.surface0));
    frame.render_widget(p, overlay);
}

/// The first visible row for a `height`-row-tall viewport that keeps `cursor` on-screen among
/// `total` rows: the window stays pinned at the top (`0`) until `cursor` would fall below it,
/// then follows `cursor` exactly (so `cursor` sits on the viewport's last visible row), capped
/// so it never scrolls past the point that shows the final `height` rows. `render_body` never
/// wraps a row across multiple terminal lines (each `Row` is exactly one `Line`), so this
/// row-count arithmetic is exactly the terminal-line arithmetic `Paragraph::scroll` needs — no
/// per-row display-height accounting required.
fn scroll_offset(total: usize, cursor: usize, height: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    let cursor = cursor.min(total - 1);
    let wants = (cursor + 1).saturating_sub(height);
    wants.min(total - height)
}

/// One row as styled spans (spec §4): the conv label (`#chan`/`@dm`) in the palette's
/// blue-family accent (`lavender`), the author in `green`, the `HH:MM` time and thread/divider
/// markers in the muted `overlay1` tone, and the message text in the default `text` fg. A
/// selected row's cursor-fill background (`cursor_bg`) is applied uniformly across every span
/// so the whole row highlights, not just one segment. A thread marker's `↳ n replies` text
/// counts as a marker (muted); a mention row's leading read/unread glyph keeps its own plain
/// `text`-fg styling (unaffected by this change, per spec) ahead of the same colored header; a
/// divider is a bare horizontal rule in the muted tone.
fn row_line(palette: &Palette, row: &Row, width: usize, selected: bool) -> Line<'static> {
    let bg = selected.then(|| palette.cursor_bg(true));
    let spans = match &row.kind {
        RowKind::Divider => vec![Span::styled("─".repeat(width), cell_style(palette.overlay1, bg))],
        RowKind::ThreadMarker { .. } => vec![
            Span::styled(row.conv_label.clone(), cell_style(palette.lavender, bg)),
            Span::styled("  ", cell_style(palette.text, bg)),
            Span::styled(row.text.clone(), cell_style(palette.overlay1, bg)),
        ],
        RowKind::Mention { read } => {
            let marker = if *read { "○" } else { "●" };
            let mut spans = vec![Span::styled(format!("{marker} "), cell_style(palette.text, bg))];
            spans.extend(header_spans(palette, row, bg));
            spans
        }
        // A reply rail row: the ordinary message header, but its leading span is the muted
        // tree connector (`├─`/`└─`, already padded by `app::reply_rail`) rather than a
        // channel label — the rail recedes so the thread reads as one indented block.
        RowKind::Reply { .. } => {
            let mut spans = header_spans(palette, row, bg);
            spans[0] = Span::styled(row.conv_label.clone(), cell_style(palette.overlay1, bg));
            spans
        }
        // The Threads digest's per-thread header: the ordinary header spans, bold across the
        // board so each thread block starts with a visually distinct anchor row.
        RowKind::ThreadHeader => header_spans(palette, row, bg)
            .into_iter()
            .map(|s| {
                let style = s.style.add_modifier(Modifier::BOLD);
                Span::styled(s.content, style)
            })
            .collect(),
        RowKind::Message => header_spans(palette, row, bg),
    };
    Line::from(spans)
}

/// The `#chan  @author  HH:MM  text` header, as separately colored spans (see `row_line`'s doc).
fn header_spans(palette: &Palette, row: &Row, bg: Option<Color>) -> Vec<Span<'static>> {
    vec![
        Span::styled(row.conv_label.clone(), cell_style(palette.lavender, bg)),
        Span::styled("  ", cell_style(palette.text, bg)),
        Span::styled(format!("@{}", row.author), cell_style(palette.green, bg)),
        Span::styled("  ", cell_style(palette.text, bg)),
        Span::styled(row.time_hhmm.clone(), cell_style(palette.overlay1, bg)),
        Span::styled("  ", cell_style(palette.text, bg)),
        Span::styled(row.text.clone(), cell_style(palette.text, bg)),
    ]
}

/// `fg` plus the selected row's cursor-fill `bg`, if any — the one-liner every `row_line` span
/// shares so a selected row's highlight always covers the whole line uniformly.
fn cell_style(fg: Color, bg: Option<Color>) -> Style {
    let style = Style::default().fg(fg);
    match bg {
        Some(bg) => style.bg(bg),
        None => style,
    }
}

/// `app.status`, with a `polling` marker appended when in fallback mode and not already named
/// in the status text (or a compact `poll-only` marker replacing it when the pane runs with no
/// app token at all — a permanent mode must not crowd the hints off a narrow split the way a
/// full status sentence would), `t: threads` and `f: focus` key hints on the Feed tab (spec
/// §3: "`t` appears in the footer" — `f` rides alongside it, since Focus and Threads are both
/// Feed-tab view-mode toggles), a context-aware `enter expand/collapse thread` hint (spec §5) whenever
/// `App::selected_is_thread_related` says Enter would actually do something thread-related, and
/// a `g/G: top/bottom` nav-key hint on every tab (spec §2 — jumping applies uniformly now that
/// ordering is unified newest-at-bottom everywhere) — this pane has no separate footer row, so
/// every hint rides along on the status line, the bottommost row a user's eye lands on
/// regardless.
fn render_status(frame: &mut Frame, palette: &Palette, app: &App, area: Rect) {
    let mut text = app.status.clone();
    if app.poll_only {
        // The permanent no-app-token mode gets one compact marker, replacing the generic
        // `polling` one (`poll_only` implies `polling` for the pane's whole life, and a
        // doubled `poll-only · polling` would say the same thing twice).
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str("poll-only");
    } else if app.polling && !text.contains("polling") {
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str("polling");
    }
    if app.tab == Tab::Feed {
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str("t: threads");
        text.push_str(" · ");
        text.push_str("f: focus");
    }
    if app.selected_is_thread_related() {
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str("enter expand/collapse thread");
    }
    if !text.is_empty() {
        text.push_str(" · ");
    }
    text.push_str("g/G: top/bottom");
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(palette.peach).bg(palette.surface0)),
        area,
    );
}

/// The active tab's body viewport height for a `total_rows`-tall terminal frame: the tab bar and
/// status line each claim one fixed row (see `render_ready`'s `Layout::vertical`), so the body
/// gets whatever's left, floored at `0` for a frame too short to fit both chrome rows. Exposed so
/// Task 7's event loop can compute the same height *before* calling `terminal.draw` (from a raw
/// `crossterm::terminal::size()`, with no `Frame` yet available) and feed it to
/// `App::set_viewport_rows` ahead of a `page_move` needing it.
#[must_use]
pub fn body_rows(total_rows: u16) -> usize {
    total_rows.saturating_sub(2) as usize
}

/// The OSC 0 terminal-title escape naming the unread mention count — the nav-presence spike
/// (spec §Nav presence). The event loop emits this to stdout, before the ratatui draw,
/// whenever `App::unread_mentions()` changes, on the chance herdr's left-nav panel reflects a
/// terminal-title update; unverified beyond this escape string (a live-smoke question, not a
/// render-test one — see the report).
#[must_use]
pub fn nav_title(unread: usize) -> String {
    format!("\x1b]0;slack ({unread})\x07")
}

#[cfg(test)]
mod tests {
    use super::{nav_title, scroll_offset};

    #[test]
    fn nav_title_builds_the_osc_0_escape_with_the_unread_count() {
        assert_eq!(nav_title(3), "\x1b]0;slack (3)\x07");
    }

    #[test]
    fn scroll_offset_is_zero_when_every_row_already_fits() {
        assert_eq!(scroll_offset(5, 3, 10), 0);
    }

    #[test]
    fn scroll_offset_stays_pinned_to_the_top_while_the_cursor_fits_in_view() {
        assert_eq!(scroll_offset(100, 0, 10), 0);
        assert_eq!(scroll_offset(100, 9, 10), 0);
    }

    #[test]
    fn scroll_offset_follows_the_cursor_once_it_would_fall_below_the_viewport() {
        assert_eq!(scroll_offset(100, 10, 10), 1);
        assert_eq!(scroll_offset(100, 99, 10), 90);
    }

    #[test]
    fn scroll_offset_never_scrolls_past_the_final_screenful() {
        assert_eq!(scroll_offset(100, 50, 10), 41);
        assert_eq!(scroll_offset(20, 19, 10), 10);
    }
}
