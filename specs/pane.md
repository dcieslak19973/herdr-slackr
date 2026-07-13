---
Status: Current
Created: 2026-07-12
Last edited: 2026-07-12
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

Row shapes, identical structure across both tabs:

| kind          | rendered as                                                     |
| -------------- | ------------------------------------------------------------------|
| message       | `#chan  @author  HH:MM  text` (`@chan` prefix for a DM)          |
| thread marker | `#chan  ↳ n replies` — a collapsed thread's root, in place of its replies |
| divider       | a bare horizontal rule                                            |
| mention       | `●`/`○` read marker, then the same message header as above        |

**Row colors:** each row renders as separately styled spans, not one flat color — consistent
across every configured palette (see `config.md`'s theme list):

| segment                        | palette field | applies to                                          |
| -------------------------------- | --------------- | -------------------------------------------------------|
| conversation label (`#chan`/`@dm`) | `lavender`    | message, thread marker, mention                        |
| author (`@name`)                | `green`       | message, mention                                        |
| time (`HH:MM`) / thread markers / divider | `overlay1` | message, thread marker (`↳ n replies`), divider |
| message text                    | `text` (default fg) | message, mention                                 |

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
| T2  | Each qualifying thread renders as its root row immediately followed by every locally-known reply, in chronological order, nested with the same `↳ ` prefix the Timeline uses for an expanded thread or an orphaned reply — **always shown regardless of the Timeline's `expanded` flag** for that thread; there is no collapsed state in this view. |
| T3  | Threads are ordered by latest activity descending: the newest locally-known reply's `ts`, or the root's own `ts` if it has no reply yet — a thread that just received a reply jumps back to the top even if its root is old. |
| T4  | `Enter` on any row in this view (re)fetches that thread's replies via `conversations.replies` unconditionally — not the Timeline's expand/collapse toggle, and it never reads or flips `App`'s `expanded` set. A reply row's Enter resolves to the thread it belongs to, same as its root's. |
| T5  | A reply whose root was never backfilled or otherwise seen still gets an entry — a *synthetic* one, headed `(thread — root not loaded)` in place of the normal `#chan @author HH:MM text` header, grouped with every other reply sharing that same unknown root. Selecting it and pressing `Enter` fetches the real root via the same `conversations.replies` call (Slack returns the root as the first message); once the fetch lands, the very next redraw naturally shows the real root row in place of the placeholder — no explicit cleanup step. |
| T6  | Root and reply rows in this view share `SelKind::Message` identity with their Timeline counterparts (same `(conv, ts)`), so a selection made in one view still resolves in the other if the row exists there too. |
| T7  | Switching views (`t`) resyncs cursor/selection into the new projection exactly like switching tabs (P8): the old selected identity is kept if it still names a row in the new view, otherwise cursor/selection are clamped/re-derived from scratch. |

## Behavior

| #  | Always true                                                                                                     |
| -- | --------------------------------------------------------------------------------------------------------------- |
| P1 | The Feed tab is one chronological stream across every subscribed conversation, ordered by message `ts`.         |
| P2 | A thread's replies render collapsed under its root as one `ThreadMarker` row unless expanded; `Enter` on that row toggles it, fetching replies via REST on first expand. |
| P3 | A reply whose root the pane never backfilled still renders, as a normal row prefixed `↳`, rather than vanishing. |
| P4 | An unread divider sits before the first Feed row that arrived since the last keypress in the pane — any key, not only navigation, since a terminal child process has no other attention signal. |
| P5 | The Mentions tab holds only messages that trigger attention: a literal `@self` token, any Im/Mpim message, or a keyword hit — newest first. |
| P6 | Each Mentions row carries its own read/unread marker, toggled independently by `Enter`; toggling never touches Slack (O1 in `overview.md`). |
| P7 | Selection is identity-based: moving the cursor records the selected row's `(conv, ts)` (and, for a thread marker sharing its root's id, which of the two it is), so a later insert/delete re-finds the same row instead of retargeting whatever now sits at the old index. |
| P8 | Switching tabs re-derives cursor and selection for the new tab's row list; a cursor position valid in a longer tab is clamped into a shorter one. |
| P9 | Message text is plain text: Slack mrkdwn styling is not interpreted, but `<@U…>`, `<#C…\|name>`, `<url\|label>`, and HTML escapes are resolved to display form. |

**Keys:**

| key                  | action                                                          |
| --------------------- | ------------------------------------------------------------------|
| `1` / `2`            | switch to Feed / Mentions                                        |
| `Tab`                | switch tab (Feed ↔ Mentions)                                      |
| `j`/`k`, `↓`/`↑`      | move the cursor                                                   |
| `Enter`               | Feed Timeline: expand/collapse the selected thread. Feed Threads view: (re)fetch the selected thread's replies. Mentions: toggle read |
| `t`                   | Feed tab only: toggle between the Timeline and the Threads-only view (see below) |
| `o`                   | open the selected row's Slack permalink (`chat.getPermalink`) in the browser |
| `r`                   | manual refresh: re-pull the last 50 messages of every subscribed conversation |
| `q`                   | quit the pane                                                     |

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

herdr's plugin v1 has no nav extension point for a custom badge. The pane emits an OSC 0 terminal-title escape (`slack (n)`, `n` the unread mention count) to stdout whenever the count changes, on the chance herdr's left-nav panel reflects a terminal-title update the way it does for another pane observed doing so. Whether it does is unverified pending a live-herdr check (see the project README's Limitations section); until confirmed, the tab bar's own count is the only reliable indicator.

## Related specs

- [overview](./overview.md)
- [config](./config.md)
- [slack-host](./slack-host.md)
