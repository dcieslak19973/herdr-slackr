---
Status: Current
Created: 2026-07-12
Last edited: 2026-07-23
---

# The pane

The Feed/Mentions UI: tabs, keys, row markers, and the two degraded states.

## Overview

One frame, three regions, always: a one-line tab bar, the active tab's row list, and a one-line status bar.

```
 1 Feed  2 Mentions (3)
 #eng-infra  @dan  09:14  shipping the migration now
 #eng-infra  ↳ 2 replies
 ───────────────────────────────────────────────────
 @priya  09:20  can you look at this when you get a sec
```

| region     | content                                                                       |
| ----------- | ------------------------------------------------------------------------------ |
| tab bar    | `1 Feed` / `2 Mentions (n)`, active tab underlined; `n` is the unread mention count |
| row list   | the active tab's rows, cursor row highlighted                                  |
| status bar | one-line notice (socket down, rate limit, theme warning); empty when nothing to say |

Message-family rows (message, reply, mention) **wrap**: text longer than the pane width breaks
at word boundaries (display-width-measured, so wide glyphs count double; a single over-wide
word breaks mid-word) onto continuation lines indented to the text column, and an explicit
newline in the message text is a forced break — a multi-paragraph Slack message renders as its
paragraphs. Summary rows (thread marker, digest header, overflow, divider) stay one clipped
line by design: their text is a preview, not the content. Cursor/scroll math tracks rows in
*display lines* (`ui::scroll_offset_lines`): the selected row is always fully visible, or
pinned to its first line when taller than the viewport.

Row shapes, identical structure across both tabs:

| kind          | rendered as                                                     |
| -------------- | ------------------------------------------------------------------|
| message       | `#chan  @author  HH:MM  text  👍3 :parrot:` — reactions, when any, as a trailing muted suffix (`(name, count)` pairs in Slack's order; count omitted at 1; common shortcodes render as Unicode via the vendored `emoji` table, custom/unknown ones as `:name:`, skin-tone suffixes stripped). Last on the line so a flaky emoji glyph width can only misalign what follows it — nothing. Applies to message, reply, mention, and thread-header rows alike. |
| thread marker | `#chan  ↳ n replies · @author: <latest reply text>` — a collapsed thread's one-row summary: count first (fixed position, clip-safe at any width), then the latest locally-known reply's resolved author and text; just `↳ n replies` when no reply is stored yet. Replaces the old scattered per-reply activity rows entirely (see P2a). |
| reply (rail)  | `├─  @author  HH:MM  text` — a reply beneath its root (Timeline expanded, Threads view), its conv label swapped for a tree-connector rail (`└─` on the thread's last reply) space-padded to the root row's label width so author/time/text columns align with the root's |
| thread header | the Threads view's per-thread anchor: `#chan  @author  <last-activity time>  n replies · <root text>`, all spans bold — the time column is the thread's *latest activity* (the view's sort key), not the root's own age |
| overflow      | `… n earlier replies` — the Threads view's muted stand-in for replies past the last-3 cap (T2); carries the root's id, so `Enter` on it refetches the full thread |
| divider       | a bare horizontal rule                                            |
| mention       | `●`/`○` read marker, then the same message header as above        |

**Row colors:** each row renders as separately styled spans, not one flat color — consistent
across every configured palette (see `config.md`'s theme list):

| segment                        | palette field | applies to                                          |
| -------------------------------- | --------------- | -------------------------------------------------------|
| conversation label (`#chan`/`@dm`) | `lavender`    | message, thread marker, mention, thread header (bold) |
| author (`@name`)                | `green`       | message, reply, mention, thread header (bold)          |
| time (`HH:MM`) / thread markers / divider / reply rail (`├─`/`└─`) / overflow | `overlay1` | message, thread marker, divider, reply, overflow |
| message text                    | `text` (default fg) | message, reply, mention, thread header (bold)    |

A selected row's cursor highlight fills the whole line uniformly (all spans share the same
background), so the color-segmentation never competes with the selection indicator. A mention
row's leading `●`/`○` read marker keeps the plain `text` fg ahead of the same colored header.

## Threads view

`t` toggles the Feed tab (Feed only — a no-op on the Mentions tab) between two projections of the
same message store, tracked as `FeedView`:

| view       | shows                                                                                       |
| ----------- | -----------------------------------------------------------------------------------------------|
| Timeline   | the row shapes above: every message, threads collapsed under a `ThreadMarker` unless expanded |
| Threads    | threads only — non-threaded messages are excluded entirely                                     |

**Threads view behavior:**

| #   | Always true                                                                                       |
| --- | ---------------------------------------------------------------------------------------------------|
| T1  | Only qualifying threads appear — a root whose reply count (Slack's `reply_count` metadata or the number of locally-known replies, whichever is greater) is nonzero. A root with zero replies either way is excluded, same as any other non-threaded message. |
| T2  | Each qualifying thread renders as its bold *thread header* row (count-first text, latest-activity time — see the row-shapes table) immediately followed by its newest **three** locally-known replies as connector-rail rows in chronological order; when more exist, one muted `… n earlier replies` overflow row (carrying the root's id — Enter refetches, T4) stands in for the rest. Shown regardless of the Timeline's `expanded` flag; there is no collapsed state in this view. The digest is a triage surface, not an archive — a 40-reply thread must not push every other thread off screen. |
| T3  | Threads are ordered by latest activity ascending — oldest activity first, newest at the bottom, unified with every view's newest-at-the-bottom direction (P1a): the newest locally-known reply's `ts`, or the root's own `ts` if it has no reply yet — a thread that just received a reply jumps back to the bottom even if its root is old. |
| T4  | `Enter` on any row in this view (re)fetches that thread's replies via `conversations.replies` unconditionally — not the Timeline's expand/collapse toggle, and it never reads or flips `App`'s `expanded` set. A reply row's Enter resolves to the thread it belongs to, same as its root's. |
| T5  | A reply whose root was never backfilled or otherwise seen still gets an entry — a *synthetic* one, its header reading `n replies · (thread — root not loaded)` in place of the root's text, grouped with every other reply sharing that same unknown root. Selecting it and pressing `Enter` fetches the real root via the same `conversations.replies` call (Slack returns the root as the first message); once the fetch lands, the very next redraw naturally shows the real root row in place of the placeholder — no explicit cleanup step. |
| T6  | Root and reply rows in this view share `SelKind::Message` identity with their Timeline counterparts (same `(conv, ts)`), so a selection made in one view still resolves in the other if the row exists there too. |
| T7  | Switching views (`t`) resyncs cursor/selection into the new projection exactly like switching tabs (P8): the old selected identity is kept if it still names a row in the new view, otherwise cursor/selection are clamped/re-derived from scratch. |

## Focus view

`f` toggles the Feed tab (Feed only — a no-op on the Mentions tab) into and out of a third
`FeedView`, **Focus** — a live-only filter over the same message store, narrower than either the
Timeline or Threads.

| view    | shows                                                                                          |
| -------- | ------------------------------------------------------------------------------------------------|
| Focus   | messages that arrived live during this session *and* qualify (allow-list or keyword — see below), oldest at the top / newest at the bottom, rendered with the Timeline's plain row shapes (no `ThreadMarker`/divider synthesis) |

**Focus view behavior:**

| #   | Always true                                                                                        |
| --- | -----------------------------------------------------------------------------------------------------|
| FC1 | A message qualifies for Focus only if it arrived *strictly after* `session_watermark` — the `arrival_seq` value as of the moment `App::build`'s startup backfill finished. Every backfilled message's arrival is at or below that watermark, so a strict (not `>=`) comparison excludes the whole backfill, including a message backfilled at the exact instant the watermark was captured; every message upserted afterward (socket, poll, out-of-cap DM scan) is strictly newer, so it is eligible. `session_watermark` is fixed once per pane session — restarting the pane re-backfills and resets it. |
| FC2 | Given FC1 passes, a message additionally qualifies if its conversation is an allow-listed DM/MPIM (`dm_allow`, config.md C13) **or** its text hits a `focus_keywords` entry (case-insensitive substring, same rule as the Mentions `keywords` check, config.md C14) — the two conditions are OR'd; either alone is sufficient. |
| FC3 | Focus never deletes or hides anything from the underlying store — toggling back to the Timeline still shows every message, qualifying or not, exactly as before Focus was entered. |
| FC4 | `Enter` in the Focus view behaves exactly like the Timeline's expand/collapse toggle (T-view's own `Enter` is the one exception) — Focus reuses the Timeline's row rendering and `expand_target_root` resolution unchanged. |
| FC5 | `t` and `f` are mutually exclusive Feed-tab view toggles, each driven by its own key rather than a single shared cycle: every key press sets its own target view, using `Timeline` as the "off" state for whichever other mode happened to be active. Decision table (row = view before the press, column = key pressed): |

| before → key | `t`        | `f`        |
| ------------- | ---------- | ---------- |
| `Timeline`    | `Threads`  | `Focus`    |
| `Threads`     | `Timeline` | `Focus`    |
| `Focus`       | `Threads`  | `Timeline` |

So `t` pressed while already in `Focus` lands on `Threads` (not back through `Timeline` first),
and symmetrically `f` pressed while in `Threads` lands on `Focus` directly.

| #   | Always true (continued)                                                                            |
| --- | -----------------------------------------------------------------------------------------------------|
| FC6 | Toggling into or out of Focus resyncs cursor/selection into the new projection exactly like `t` does for Threads (T7/P8): the old selected identity is kept if it still names a row in the new view, otherwise cursor/selection are clamped/re-derived from scratch. |
| FC7 | An empty Focus view (no message has qualified yet this session) renders as a normal empty row list — no special placeholder text — same as any other view with zero rows. |

## Behavior

| #  | Always true                                                                                                     |
| -- | --------------------------------------------------------------------------------------------------------------- |
| P1 | The Feed tab is one chronological stream across every subscribed conversation, ordered by message `ts`.         |
| P1a | Every tab/projection (Feed Timeline, Feed Threads, Mentions) orders its rows ascending — oldest at the top, newest at the bottom — the same direction, unified across the pane. |
| P2 | A thread's replies render collapsed under its root as one enriched `ThreadMarker` row unless expanded; `Enter` toggles it, fetching replies via REST on first expand. `Enter` toggles the same thread whether the selection is the `ThreadMarker` row itself, the root message's own row (when it has at least one reply), or a connector-rail reply row within an already-expanded thread — see `App::expand_target_root`. |
| P2a | A reply to a *collapsed* thread surfaces exclusively through its root's enriched marker — `↳ n replies · @author: <latest text>` — never as its own chronological row: the feed stays one row per thread no matter how busy the thread gets. Freshness the old per-reply activity rows carried lives in the marker's snippet instead, and the marker's arrival counter is the newest reply's, so the unread divider (P4) still lands on the thread when new replies are unseen and the `↓ n new` counter still counts them. Once expanded, replies nest under the root as `Reply` rail rows (row-shapes table). |
| P2b | Expanding/collapsing a thread sets a one-line status: `thread expanded — n replies` (singular `thread expanded — 1 reply` for exactly one) or `thread collapsed`. A failed replies fetch sets `replies failed: …` instead and leaves the expand/collapse state unchanged. |
| P3 | A reply whose root the pane never backfilled still renders, as a normal row prefixed `↳`, rather than vanishing. |
| P4 | An unread divider sits before the first Feed row that arrived since the last keypress in the pane — any key, not only navigation, since a terminal child process has no other attention signal. |
| P5 | The Mentions tab holds only messages that trigger attention: a literal `@self` token, any Im/Mpim message, or a keyword hit — ascending, oldest at the top / newest at the bottom (P1a; **changed** from newest-first). |
| P6 | Each Mentions row carries its own read/unread marker, toggled independently by `Enter`; toggling never touches Slack (O1 in `overview.md`). |
| P7 | Selection is identity-based: moving the cursor records the selected row's `(conv, ts)` (and, for a thread marker sharing its root's id, which of the two it is), so a later insert/delete re-finds the same row instead of retargeting whatever now sits at the old index. |
| P8 | Switching tabs re-derives cursor and selection for the new tab's row list; a cursor position valid in a longer tab is clamped into a shorter one. |
| P9 | Message text is plain text: Slack mrkdwn styling is not interpreted, but `<@U…>`, `<#C…\|name>`, `<url\|label>`, and HTML escapes are resolved to display form. |
| P10 | A per-tab `pending_new` counter (`App::pending_new`) counts arrivals since the cursor last left the active tab's bottom row; an arrival landing while the cursor already sits at the bottom instead follows it there (like a chat client scrolled to "now") and never touches the counter. Reaching the bottom by any means — `j`/`↓`, `G`/`End`, a page move, or the follow-snap itself — clears it to `0`. The counter increments on any arrival anywhere in the message store while scrolled up on the active tab, not only one that would add a visible row to that tab (e.g. scrolled up on Mentions, a plain channel message that isn't a mention still counts, even though it never becomes a Mentions row) — global, not tab-scoped to what actually rendered. |
| P11 | While `pending_new` is nonzero, a `↓ n new` overlay renders at the bottom-right of the body viewport, in the muted marker accent; hidden whenever `pending_new` is `0`. |
| P12 | A row's `HH:MM` timestamp is relative to the current moment (`format_ts`): the same UTC calendar day renders unchanged as `HH:MM`; any earlier UTC calendar day renders dated, as `Mon DD HH:MM` (e.g. `Jul 12 06:00`), so a message from a prior day is never confused with one from today. A malformed `ts` renders as the Unix epoch (`00:00`, no date) rather than panicking. |

**Keys:**

| key                  | action                                                          |
| --------------------- | ------------------------------------------------------------------|
| `1` / `2`            | switch to Feed / Mentions                                        |
| `Tab`                | switch tab (Feed ↔ Mentions)                                      |
| `j`/`k`, `↓`/`↑`      | move the cursor                                                   |
| `G` / `End`           | jump to the newest row (bottom) — `App::jump_newest`              |
| `g` / `Home`          | jump to the oldest row (top) — `App::jump_first`                  |
| `PageDown` / `PageUp` | move a full page (`±viewport_rows`) — `App::page_move`            |
| `Ctrl-d` / `Ctrl-u`   | move a half page (`±viewport_rows / 2`) — `App::page_move`        |
| `Enter`               | Feed Timeline: expand/collapse the selected thread (root/marker/rail row — P2). Feed Threads view: (re)fetch the selected thread's replies (header, rail, or overflow row alike). Mentions: toggle read |
| `t`                   | Feed tab only: toggle between the Timeline and the Threads-only view (see below) |
| `f`                   | Feed tab only: toggle into/out of the Focus view (see above) — mutually exclusive with `t`'s Threads toggle |
| `o`                   | open the selected row's Slack permalink (`chat.getPermalink`) in the browser |
| `r`                   | manual refresh: one immediate poll batch, plus arming the paced catch-up sweep (`App::request_refresh`) so every subscribed conversation gets one watermarked fetch under the normal request budget; re-pressing mid-sweep never shrinks the remaining count; sets a `refreshing n conversations` status |
| `q`                   | quit the pane                                                     |

`viewport_rows` is set once per draw from the body area's measured height (`App::set_viewport_rows`, fed by `ui::body_rows`), so a page move is always sized off the pane's actual on-screen row count, including right after a terminal resize.

## Degraded states

The pane has exactly two render states: `Ready` (the tab bar/rows/status above) and `Blocked` — a full-pane word-wrapped message, no chrome, shown instead of everything above.

| trigger                                                     | `Blocked` message                                        |
| ------------------------------------------------------------ | ----------------------------------------------------------|
| invalid/missing `config.toml` (see `config.md`)              | the config error, naming the path and the bad key/value    |
| invalid/missing tokens (see `config.md`)                      | the token error, naming the env var and `tokens.toml` path |
| `App::build`'s own REST failure (e.g. an unknown channel, `invalid_auth`) | the build error verbatim                        |

`Blocked` never crashes the process — `q` still quits it. Once the pane reaches `Ready`, it stays there for the rest of the session: a later Slack-side error (rate limit, socket down) surfaces in the status bar, not by reverting to `Blocked`.

An unrecognized `theme` value is not a `Blocked` trigger (see `config.md` C6): the pane starts `Ready` on the default palette with a one-line status warning.

## Nav presence

| #  | Always true                                                                                                     |
| -- | --------------------------------------------------------------------------------------------------------------- |
| N1 | On herdr ≥ 0.7.4, the pane reports its sidebar row title (`slack (n)` / bare `slack` at zero) and the tokens `slack_mentions` + `slack_link` (`live`/`polling`/`lossy`) via `herdr pane report-metadata`, source `plugin:dcieslak19973.slackr` — at most one call per `(unread, link)` change, spawned fire-and-forget, never blocking the event loop (`herdr_meta::Reporter`). |
| N2 | The first failed report writes one plugin-log line and disables the reporter for the session. Failure never surfaces in the status bar and never triggers `Blocked` (O4): the badge is decoration, not function. Unset `$HERDR_PANE_ID` disables it from the start. |
| N3 | The OSC 0 terminal-title escape (`slack (n)` on every unread change) is kept as the pre-0.7.4 fallback; whether any herdr version renders it in the nav remains unverified. |

## Related specs

- [overview](./overview.md)
- [config](./config.md)
- [slack-host](./slack-host.md)
