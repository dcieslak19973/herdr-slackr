# DM cap fix, allow-list, focus filtering, dated timestamps

**Date:** 2026-07-13
**Status:** Approved
**Repo:** `dcieslak19973/herdr-slackr`, branch `focus-and-fixes`
**Extends:** all prior specs; invariants hold. Target release 0.1.5.

## Problems (user-reported, live)

1. `dm_limit` was meant to cap only backfill/history, but in **polling mode** a DM
   outside the cap is never subscribed at all, so a new message in it is invisible —
   only socket mode already got this right (spec 2026-07-12-rate-limit-hardening
   §1 said "live socket events still surface out-of-cap DMs," but polling never
   subscribes to them in the first place).
2. No way to always-include specific people's DMs regardless of the cap.
3. No way to filter attention down to current work by keyword, without hiding
   anything permanently.
4. Timestamps show only `HH:MM`, ambiguous for anything not from today.

## Fixes

### 1. `dm_limit` caps history only, never blocks live arrivals — in both modes

The cap continues to select which DMs get **backfilled and polled for history**
(`resolve_channels`, unchanged selection logic). Separately, both delivery paths gain
an always-on side channel for out-of-cap DMs' *new* messages:

- **Socket mode** already does this correctly (any `message.im`/`message.mpim` event
  updates the model regardless of subscription) — no change needed there.
- **Polling mode**: each tick, after the capped conversation batch, issue one
  additional lightweight call — `conversations.list` is already fetched at `build`
  time with `updated` timestamps; poll instead calls `users.list`-adjacent
  `conversations.list` no more than once per **5 minutes** (a separate, longer-period
  cursor) to detect any DM/MPIM whose `updated` moved since last seen. A newly-active
  out-of-cap DM gets **one** `history(oldest=last-known-updated)` call outside the
  normal 8-slot budget (capped at 1 extra call per tick, so worst case adds 1 to the
  request count, not N). This is the "previous messages stay capped, new ones always
  arrive" behavior in polling mode too.

### 2. `dm_allow`: always-subscribed DMs regardless of the cap

New config key `dm_allow: Vec<String>` (Slack usernames or display names, default
empty). At `resolve_channels` time, DMs whose counterpart user's name **exactly**
matches an entry (case-insensitive; no substring matching — avoids surprise
over-inclusion) are included **unconditionally** — before the `dm_limit` cut, and
never evicted by it. `dms = false` still suppresses all DMs including allow-listed
ones (an explicit "no DMs" wins over an allow-list — the simpler, safer reading).
`dm_limit` continues to apply only to the *remaining* (non-allow-listed) DM pool.

### 3. Focus mode: a toggle-able live-only filter

New config key `focus_keywords: Vec<String>` (default empty; distinct from the
existing `keywords` used for Mentions-tab triggers — reuse is tempting but conflates
"notify me" with "focus filter," so kept separate per the user's framing of two
distinct needs). A message **qualifies for Focus** when it arrived live during this
session (socket delivery, or the new out-of-cap poll path, or a normal in-cap poll
tick — anything that wasn't part of startup backfill) AND matches `dm_allow` (its
conversation is an allow-listed DM) OR `focus_keywords` (case-insensitive substring in
its text, same matching rule as the existing Mentions keyword check).

- **Key**: `f` toggles Focus mode on the Feed tab (parallel to `t` for Threads;
  mutually exclusive — Focus and Threads are both Feed-tab view modes,
  `FeedView::{Timeline, Threads, Focus}`).
- **Rendering**: Focus mode reuses the timeline row layout, filtered to qualifying
  messages only, oldest-to-newest (same ascending convention). Non-qualifying
  messages are never shown in Focus, but nothing is deleted from the model — toggling
  back to Timeline shows everything as before.
- **"Since app start"**: qualification is tracked via the existing `arrival_seq`
  ordering — backfilled messages get no arrival marker (or one before the "session
  start" watermark recorded at the end of `build`); anything with `arrival_seq` at or
  after that watermark qualifies structurally, then the allow-list/keyword filter
  narrows further.
- Threads view is unaffected (no Focus variant of it — out of scope).

### 4. Dated timestamps for prior days

`ts_to_hhmm` (renamed `format_ts` for clarity) takes an injected "now" reference
(`SystemTime`, testable) and a message ts: same-UTC-calendar-day → `HH:MM` as today;
any earlier day → `Mon DD HH:MM` (e.g. `Jul 11 14:32`), reusing the existing
Howard-Hinnant civil-date math already in the codebase (from the epoch-seconds→date
conversion used elsewhere) rather than adding a date/time crate — the closed
dependency list stays closed. "Now" is UTC (unchanged existing limitation, documented
already). Applies everywhere a message row renders a time: Feed, Mentions, Threads,
Focus.

## Non-goals

- No per-user focus profiles/presets; one `focus_keywords` + one `dm_allow` list.
- No retroactive Focus over backfilled history (explicit user constraint: new
  messages only).
- No CLI changes (mentions/feed subcommands untouched — dm_allow could be added there
  later but isn't requested).
- No change to the existing Mentions `keywords` config key or its semantics.

## Testing

Pure: dm_allow inclusion bypassing the cap (unit, extends `resolve_channels` tests);
out-of-cap poll detection cadence + 1-call cap (pure scheduling fn, extends the
`next_batch` family); Focus qualification (arrival-watermark ∧ (allow-list ∨
keyword), each combination); `format_ts` same-day vs prior-day formatting incl. UTC
day boundary edge cases. Render: Focus mode toggle, dated-timestamp rows, `f` footer
hint. Existing suite (305 tests) stays green.
