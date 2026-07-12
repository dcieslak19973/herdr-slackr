//! Rendering the Feed/Mentions pane: tab bar, row list, status line. See
//! `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md` §The pane.
//!
//! Two states: a normal frame over an [`App`] ([`PaneState::Ready`]), or a full-pane remedy
//! screen naming what's missing ([`PaneState::Blocked`]) — missing/invalid tokens or config
//! never crash the pane, they show a fixable message instead (reviewr's degraded-state
//! pattern). Rendering reads `App` only; all state changes live in `app.rs`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, Row, RowKind, Tab};
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

/// `1 Feed  2 Mentions (n)`, the active tab underlined and bright.
fn render_tab_bar(frame: &mut Frame, palette: &Palette, app: &App, area: Rect) {
    let n = app.unread_mentions();
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("1 Feed", tab_style(palette, app.tab == Tab::Feed)),
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
        Tab::Feed => app.feed_rows(),
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

/// One row as `#chan  @author  HH:MM  text` (a thread marker keeps its own `↳ n replies`
/// text; a divider is a bare horizontal rule; a mention row gets a leading read/unread
/// marker ahead of the same header).
fn row_line(palette: &Palette, row: &Row, width: usize, selected: bool) -> Line<'static> {
    let mut style = Style::default().fg(palette.text);
    if selected {
        style = style.bg(palette.cursor_bg(true));
    }
    let text = match &row.kind {
        RowKind::Divider => "─".repeat(width),
        RowKind::ThreadMarker { .. } => format!("{}  {}", row.conv_label, row.text),
        RowKind::Mention { read } => {
            let marker = if *read { "○" } else { "●" };
            format!("{marker} {}", header_and_text(row))
        }
        RowKind::Message => header_and_text(row),
    };
    Line::from(Span::styled(text, style))
}

fn header_and_text(row: &Row) -> String {
    format!("{}  @{}  {}  {}", row.conv_label, row.author, row.time_hhmm, row.text)
}

/// `app.status`, with a `polling` marker appended when in fallback mode and not already
/// named in the status text.
fn render_status(frame: &mut Frame, palette: &Palette, app: &App, area: Rect) {
    let mut text = app.status.clone();
    if app.polling && !text.contains("polling") {
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str("polling");
    }
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(palette.peach).bg(palette.surface0)),
        area,
    );
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
