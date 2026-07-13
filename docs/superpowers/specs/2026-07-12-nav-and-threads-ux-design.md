# Navigation and threads UX

**Date:** 2026-07-12
**Status:** Approved
**Repo:** `dcieslak19973/herdr-slackr`, branch `nav-ux`
**Extends:** all prior specs; invariants hold. Target release 0.1.4.

## Problems (user-reported, live)

Reaching recent messages is hard: no jump-to-newest, no paging, no signal when new
messages arrive while scrolled up, auto-follow yanks the cursor, and the tabs disagree
about direction (timeline newest-at-bottom; Mentions/Threads newest-at-top). Threads:
Enter only works on the `↳` marker row (undiscoverable), no feedback during/after the
fetch, and a reply to a collapsed thread is invisible in the timeline.

## Fixes

### 1. One direction everywhere: newest at the bottom

Feed timeline (already), Mentions, and Threads views all order oldest→newest top to
bottom. The bottom of every list is "now". Threads-view thread ordering becomes latest
activity **ascending** (most recently active thread last, its replies still nested
under the root). Mentions rows flip to chronological. Read markers/divider semantics
are unchanged by ordering.

### 2. Navigation keys

- `G` / `End`: jump cursor+view to the newest row. `g` / `Home`: to the first row.
- `PageDown`/`PageUp` and `ctrl-d`/`ctrl-u`: full/half-page cursor moves (clamped),
  reusing `move_cursor` (identity selection intact).
- Footer/status hints updated.

### 3. New-arrivals indicator

When rows are appended while the cursor is not on the last row, the bottom edge of the
pane shows `↓ n new` (count of arrivals since the cursor left the bottom). Reaching
the bottom (any means) clears it. Renders in the same muted accent as markers.

### 4. Follow stickiness

The view/cursor follows arrivals only when the cursor is on the last row at the moment
the arrival lands (current snap behavior, kept); otherwise nothing moves and the
indicator increments. No config knob.

### 5. Thread expansion UX

- Enter on a thread ROOT row (timeline or threads view) toggles expansion exactly like
  Enter on its marker row; both routes share the fetch path.
- Completion feedback in the status line: `thread expanded — n replies` /
  `thread collapsed` / the existing error wording.
- Context-aware footer: when the cursor is on a root-with-thread, marker, or reply
  row, the hint shows `enter expand/collapse thread`.

### 6. Reply activity in the timeline

A reply to a thread whose root is COLLAPSED renders as a compact activity row at the
reply's own chronological position: `↳ @author replied: <text>` (conv label + time
styled as normal rows). Enter on an activity row expands that thread (rendering
replies nested under the root, activity rows for it disappearing). Expanded threads
keep today's nesting. Orphan replies (unknown root) keep their existing inline
treatment (which this generalizes). The Mentions tab is unaffected (mention replies
already appear there via is_mention).

## Non-goals

No per-thread read state; no smooth scrolling; no mouse; no config knobs for any of
this; CLI untouched.

## Testing

Pure: ordering flips (mentions/threads ascending), page-move clamping, indicator
count/clear transitions, activity-row projection (collapsed vs expanded vs orphan),
enter-on-root routing. Render: indicator visibility, activity row, footer hint
variants, G/g jumps. Existing 272 tests stay green (ordering-dependent tests update
their expectations with meaning preserved).
