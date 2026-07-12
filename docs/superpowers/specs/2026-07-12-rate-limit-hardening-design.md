# Rate-limit hardening

**Date:** 2026-07-12
**Status:** Approved
**Repo:** `dcieslak19973/herdr-slackr`, branch `rate-limits`
**Extends:** the pane spec and the agent-cli spec; their invariants hold.

## Problem

Observed live: slackr hammers Slack's rate limits. Four compounding causes:
polling fallback re-fetches full history for every conversation every tick;
`dms = true` subscribes every IM/MPIM ever; 429 handling guesses a fixed 30s and the
next tick fires on schedule anyway; `users.list` is re-fetched in full by every pane
start and every CLI invocation.

## Fixes

### 1. DM cap: `dm_limit`

New config key `dm_limit` (integer, default **20**, valid `0..=200`; `0` = no DMs even
when `dms = true`). When `dms = true`, subscribe only the `dm_limit` most recently
active IMs/MPIMs, by the conversation's `updated` field from `conversations.list`
(millisecond epoch; if absent on this workspace's payloads, fall back to including the
first `dm_limit` DMs in list order and log the degradation — never scan history to
rank). Applies identically to the pane and the CLI (both go through
`resolve_channels`). The README documents that a dormant DM outside the cap still
alerts in real time via the socket (`message.im` events are not scoped by the cap —
an event for an unsubscribed DM inserts it into the model and, when it displaces
nothing, it simply appears); polling mode does not see outside the cap.

### 2. Incremental, staggered polling

- `App` records the newest seen `ts` per conversation. `poll_tick` passes it as
  `oldest` to `conversations.history` (Slack returns only newer messages; the common
  tick returns empty bodies).
- Request-count is what rate limits meter, so ticks are also **staggered**: each tick
  polls at most `POLL_BATCH` (8) conversations, round-robin across the subscribed set,
  resuming where the last tick stopped. Worst-case freshness for a conversation is
  `ceil(N/8) × poll_fallback_secs`, which the status line does not need to surface.
- A tick that hits `RateLimited(secs)` sets a **cooldown deadline**; subsequent ticks
  are skipped entirely (not merely shortened) until it passes. The status line shows
  the existing rate-limit notice while cooling down.

### 3. Honor Slack's real `Retry-After`

The REST layer appends `--write-out '\n%{http_code} %header{retry-after}'` to every
curl call (curl ≥ 7.83; on older curl the trailer is absent and parsing falls back to
current behavior). `parse_response` splits the trailer off the body: HTTP 429 (or a
body `ratelimited` error) yields `RateLimited(retry_after)` with the server's actual
seconds, defaulting to 30 when the header is missing. Everything else is unchanged.

### 4. Shared users cache

`users.list` results persist to `$HERDR_PLUGIN_STATE_DIR/users.json` (or, when that
env is unset — the CLI case — the state path derived the same way the config dir is:
`~/.local/state/herdr/plugins/dcieslak19973.slackr/users.json`), with a fetched-at
stamp. Pane build and every CLI invocation read the cache when younger than
**24 hours**, else refetch and rewrite (best-effort: an unwritable state dir degrades
to per-process fetch with a log line). Unknown user ids keep rendering raw. The cache
holds only public directory data (id → display name); still, file mode 0600 on Unix
for consistency.

### 5. Build-time resilience

A `RateLimited` during startup backfill no longer fails `build` into the Blocked
screen: build sleeps the `Retry-After` once and retries that conversation; a second
consecutive rate limit degrades to skipping the remaining backfill with the status
notice (the socket/poll path fills in later). Other per-channel history errors keep
failing build (unchanged contract: a misconfigured channel should fail loud).

## Non-goals

- No change to the CLI's partial-results contract (it benefits from #3 and #4 as-is).
- No request budget/token-bucket framework — the five fixes above are targeted.
- No persistence of message history.

## Testing

Pure/unit: `dm_limit` selection incl. `updated`-absent fallback; round-robin batch
scheduling (pure next-batch fn); cooldown gating; `--write-out` trailer parsing
(429 + header, 429 sans header, older-curl absent trailer, body-ratelimited);
users-cache TTL decisions (fresh/stale/unwritable, injected clock). Config: `dm_limit`
key contract tests. Integration: existing suites stay green; CLI uses the cache
(observable via a pre-seeded users.json in a fixture state dir).
