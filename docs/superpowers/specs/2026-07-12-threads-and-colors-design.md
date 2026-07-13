# Threads view and row colors

**Date:** 2026-07-12
**Status:** Approved
**Repo:** `dcieslak19973/herdr-slackr`, branch `threads-view`
**Extends:** the pane, agent-cli, and rate-limit specs; their invariants hold.

## Problems

1. Thread rendering only works for replies that arrived live over the socket:
   `conversations.history` returns thread roots without their replies, and the root's
   `reply_count` metadata is discarded — so backfilled/polled threads render as plain
   messages with no marker, and in polling mode new replies (and thread mentions) are
   never seen at all.
2. The user wants a thread-centric reading mode: a toggle between the timeline and a
   threads-only digest.
3. Feed rows render monochrome; channel, author, and text should be visually distinct.

## Fixes

### 1. Thread metadata from history

`Message` gains `reply_count: Option<u32>` (from history/backfill root objects; socket
`message` events for roots carry it on `message_changed` updates when Slack sends it —
parse when present, else leave prior value). The diff-pane `ThreadMarker` count becomes
`max(reply_count, locally-known replies)`, so a backfilled thread shows `↳ n replies`
immediately and Enter-to-expand (existing `conversations.replies` fetch) works on it.

### 2. Bounded thread refresh in polling mode

Polling's existing 8-call tick budget is split: up to **2 of the 8 slots** rotate
round-robin over "active threads" — threads that are currently expanded, plus roots
whose `reply_count` exceeds the locally-known reply count — issuing
`conversations.replies` with the thread's newest-known reply ts as `oldest`. Socket
mode needs nothing (replies already arrive live). New replies fetched this way run
through the normal upsert path, so thread mentions land in the Mentions tab. No new
config; the budget stays 8 calls per tick total.

### 3. Threads view

The Feed tab gains a view toggle on **`t`** (free key; footer-hinted): timeline ⇄
threads. The threads view is a digest of **only threads** — every conversation's roots
with `ThreadMarker`-worthy state (reply_count > 0 or local replies), ordered by latest
thread activity (newest reply ts, else root ts) descending, each root rendered with its
locally-known replies nested beneath (always expanded in this view; Enter on a root
(re)fetches its replies). Non-threaded messages are excluded. Cursor/selection reuse
the identity machinery (`SelKind` unchanged; the threads view is a row-list projection
like the others). The tab bar shows the mode (`1 Feed·threads` or similar minimal
marker) and `t` appears in the footer.

### 4. Row colors

Feed/Mentions/Threads rows render as styled spans from the active palette:
conversation label (`#chan`/`@dm`) in the palette's blue/sapphire accent, author in
green, the `HH:MM` time and thread/divider markers in the muted overlay tone, message
text in the default foreground. Resolved entity mentions inside text stay plain (no
inline styling — YAGNI). Applies to all 18 themes via existing palette fields; render
tests assert the styles on the test-backend buffer (the render suite already asserts
colors for chips/markers — same pattern).

## Non-goals

- No CLI changes (mentions/feed output stays text/JSON; thread replies in the CLI is a
  separate decision).
- No auto-expansion in the timeline view; no per-thread read tracking.
- No unread accounting changes: the divider/mention semantics are untouched.

## Testing

Pure: marker count max(), active-thread selection + 2-slot rotation (extends the
`next_batch` policy fns), threads-view row projection (ordering, nesting, exclusion),
color span assembly per row kind. Render: threads-view snapshot, colored-row style
assertions, `t` toggle. Existing 227 tests stay green.
