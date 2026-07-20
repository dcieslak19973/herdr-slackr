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
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    let mut heights: Vec<usize> = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let row_lines = row_lines(palette, row, width, i == app.cursor);
        heights.push(row_lines.len());
        lines.extend(row_lines);
    }
    let offset = scroll_offset_lines(&heights, app.cursor, area.height as usize);
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

/// The first visible *display line* for a `viewport`-line-tall body, given each row's wrapped
/// line count (`heights`, one entry per row) and the cursor's row index: the window stays
/// pinned at the top until the cursor row's last line would fall below it, then scrolls just
/// enough that the whole cursor row is visible — or, for a row taller than the viewport
/// itself, pins to the row's first line (its opening is the part worth showing). Capped so it
/// never scrolls past the final screenful. With every height at `1` this reduces exactly to
/// the old one-row-per-line arithmetic.
fn scroll_offset_lines(heights: &[usize], cursor: usize, viewport: usize) -> usize {
    if viewport == 0 || heights.is_empty() {
        return 0;
    }
    let total: usize = heights.iter().sum();
    if total <= viewport {
        return 0;
    }
    let cursor = cursor.min(heights.len() - 1);
    let first: usize = heights[..cursor].iter().sum();
    let end = first + heights[cursor];
    let wants = end.saturating_sub(viewport).min(first);
    wants.min(total - viewport)
}

/// Greedy display-width word wrap into lines of at most `avail` columns (measured with
/// `unicode-width`, so a double-width emoji or CJK glyph counts as two): words move whole to
/// the next line when they'd overflow, a single word wider than `avail` breaks mid-word at
/// the column limit, and an explicit `\n` in the text is a forced break (Slack messages are
/// routinely multi-paragraph; flattening their newlines into spaces would garble them).
/// Always returns at least one (possibly empty) line.
fn wrap_text(text: &str, avail: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let avail = avail.max(1);
    let mut lines = Vec::new();
    for segment in text.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in segment.split_whitespace() {
            let word_w = word.width();
            let sep = usize::from(!cur.is_empty());
            if cur_w + sep + word_w <= avail {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.push_str(word);
                cur_w += sep + word_w;
            } else if word_w <= avail {
                lines.push(std::mem::take(&mut cur));
                cur.push_str(word);
                cur_w = word_w;
            } else {
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                }
                let mut piece_w = 0usize;
                for ch in word.chars() {
                    let ch_w = ch.width().unwrap_or(0);
                    if piece_w + ch_w > avail && !cur.is_empty() {
                        lines.push(std::mem::take(&mut cur));
                        piece_w = 0;
                    }
                    cur.push(ch);
                    piece_w += ch_w;
                }
                cur_w = piece_w;
            }
        }
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// All display lines for one row. Summary rows — divider, thread marker, digest header — stay
/// a single clipped line by design (their text is a preview, not the content). Message-family
/// rows (message, reply rail, mention) wrap their text at the pane width, chat-style:
/// continuation lines indent to the text column so the message reads as one aligned block,
/// and explicit newlines in the message are honored as line breaks (see [`wrap_text`]).
fn row_lines(palette: &Palette, row: &Row, width: usize, selected: bool) -> Vec<Line<'static>> {
    match row.kind {
        RowKind::Divider | RowKind::ThreadMarker { .. } | RowKind::ThreadHeader => {
            vec![row_line(palette, row, width, selected)]
        }
        RowKind::Message | RowKind::Reply { .. } | RowKind::Mention { .. } => {
            wrapped_message_lines(palette, row, width, selected)
        }
    }
}

/// A message-family row wrapped into one-or-more display lines: the usual colored prefix
/// (mention glyph / conv label or rail / author / time) on the first line, the text wrapped at
/// the remaining width, continuation lines indented to the text column, and the muted
/// reactions span trailing the final line. A pane too narrow to afford the indented column
/// (under ~10 usable text columns) falls back to full-width continuations rather than
/// wrapping into a sliver.
fn wrapped_message_lines(
    palette: &Palette,
    row: &Row,
    width: usize,
    selected: bool,
) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    let bg = selected.then(|| palette.cursor_bg(true));
    let conv_color = if matches!(row.kind, RowKind::Reply { .. }) {
        palette.overlay1 // the thread rail is muted — see `row_line`'s Reply arm
    } else {
        palette.lavender
    };
    let mut prefix: Vec<Span<'static>> = Vec::new();
    if let RowKind::Mention { read } = row.kind {
        let marker = if read { "○ " } else { "● " };
        prefix.push(Span::styled(marker.to_string(), cell_style(palette.text, bg)));
    }
    prefix.push(Span::styled(row.conv_label.clone(), cell_style(conv_color, bg)));
    prefix.push(Span::styled("  ", cell_style(palette.text, bg)));
    prefix.push(Span::styled(format!("@{}", row.author), cell_style(palette.green, bg)));
    prefix.push(Span::styled("  ", cell_style(palette.text, bg)));
    prefix.push(Span::styled(row.time_hhmm.clone(), cell_style(palette.overlay1, bg)));
    prefix.push(Span::styled("  ", cell_style(palette.text, bg)));

    let indent: usize = prefix.iter().map(|s| s.content.as_ref().width()).sum();
    let (avail, cont_indent) =
        if indent + 10 >= width { (width.max(1), 0) } else { (width - indent, indent) };
    let chunks = wrap_text(&row.text, avail);
    let last = chunks.len() - 1;
    let mut lines = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = if i == 0 {
            let mut spans = prefix.clone();
            spans.push(Span::styled(chunk, cell_style(palette.text, bg)));
            spans
        } else {
            vec![
                Span::styled(" ".repeat(cont_indent), cell_style(palette.text, bg)),
                Span::styled(chunk, cell_style(palette.text, bg)),
            ]
        };
        if i == last && !row.reactions.is_empty() {
            spans.push(Span::styled(
                format!("  {}", row.reactions),
                cell_style(palette.overlay1, bg),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
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

/// The `#chan  @author  HH:MM  text` header, as separately colored spans (see `row_line`'s
/// doc), plus — when the row carries any — a trailing muted reactions span (`👍3 :parrot:`);
/// muted so counts inform without competing with the text, and last on the line so a flaky
/// emoji glyph width can only misalign what follows it, which is nothing.
fn header_spans(palette: &Palette, row: &Row, bg: Option<Color>) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(row.conv_label.clone(), cell_style(palette.lavender, bg)),
        Span::styled("  ", cell_style(palette.text, bg)),
        Span::styled(format!("@{}", row.author), cell_style(palette.green, bg)),
        Span::styled("  ", cell_style(palette.text, bg)),
        Span::styled(row.time_hhmm.clone(), cell_style(palette.overlay1, bg)),
        Span::styled("  ", cell_style(palette.text, bg)),
        Span::styled(row.text.clone(), cell_style(palette.text, bg)),
    ];
    if !row.reactions.is_empty() {
        spans.push(Span::styled(format!("  {}", row.reactions), cell_style(palette.overlay1, bg)));
    }
    spans
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
    use super::{nav_title, scroll_offset_lines, wrap_text};

    #[test]
    fn nav_title_builds_the_osc_0_escape_with_the_unread_count() {
        assert_eq!(nav_title(3), "\x1b]0;slack (3)\x07");
    }

    // ---- wrap_text: greedy display-width word wrap with forced breaks at newlines ----------

    #[test]
    fn wrap_text_returns_one_line_when_it_fits() {
        assert_eq!(wrap_text("short", 20), vec!["short"]);
        assert_eq!(wrap_text("", 20), vec![""]);
    }

    #[test]
    fn wrap_text_wraps_greedily_at_word_boundaries() {
        assert_eq!(wrap_text("one two three four", 9), vec!["one two", "three", "four"]);
    }

    #[test]
    fn wrap_text_breaks_a_word_longer_than_the_width() {
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_text_measures_display_width_not_char_count() {
        // 你 is double-width: three of them (display width 6) cannot fit in 5 columns.
        assert_eq!(wrap_text("你你你", 5), vec!["你你", "你"]);
    }

    #[test]
    fn wrap_text_honors_explicit_newlines_as_forced_breaks() {
        assert_eq!(wrap_text("first line\nsecond", 40), vec!["first line", "second"]);
        assert_eq!(wrap_text("a\n\nb", 40), vec!["a", "", "b"], "a blank line survives");
    }

    // ---- scroll_offset_lines: the row-scroll math generalized to variable-height rows -------

    #[test]
    fn scroll_offset_lines_matches_the_old_row_math_when_every_row_is_one_line() {
        let h = vec![1; 100];
        assert_eq!(scroll_offset_lines(&h, 0, 10), 0);
        assert_eq!(scroll_offset_lines(&h, 9, 10), 0);
        assert_eq!(scroll_offset_lines(&h, 10, 10), 1);
        assert_eq!(scroll_offset_lines(&h, 50, 10), 41);
        assert_eq!(scroll_offset_lines(&h, 99, 10), 90);
        let short = vec![1; 5];
        assert_eq!(scroll_offset_lines(&short, 3, 10), 0, "everything fits — no scroll");
    }

    #[test]
    fn scroll_offset_lines_keeps_a_wrapped_cursor_row_fully_visible() {
        // Rows of 1,1,3,1 lines; viewport 4. Cursor on the 3-line row (lines [2,5)):
        // scrolling to offset 1 shows lines 1..5 — the whole wrapped row.
        let h = vec![1, 1, 3, 1];
        assert_eq!(scroll_offset_lines(&h, 2, 4), 1);
        // Cursor on the last row (lines [5,6)): offset 2 shows lines 2..6.
        assert_eq!(scroll_offset_lines(&h, 3, 4), 2);
    }

    #[test]
    fn scroll_offset_lines_pins_to_the_top_of_a_row_taller_than_the_viewport() {
        let h = vec![1, 5, 1];
        assert_eq!(scroll_offset_lines(&h, 1, 3), 1, "show the row's first lines, not its tail");
    }
}
