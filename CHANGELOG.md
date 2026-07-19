# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **Compact footer.** Poll-only mode announced itself with a permanent status sentence
  (`poll-only mode — no app token, live socket disabled · polling · …`) that crowded the key
  hints off a narrow split; it is now a single `poll-only` marker that replaces (never doubles
  with) the generic `polling` one. The Feed hints also tightened to name their destinations —
  `t: threads · f: focus` instead of `t: toggle view · f: toggle focus`.

## [0.1.10] — 2026-07-19

### Added
- **Poll-only mode: the `xapp-` app token is now optional.** Omitting it (no env var, no
  `app_token` key) starts the pane with no Socket Mode connection: the request-budgeted polling
  fallback becomes the permanent delivery path, `r` still forces a full sweep, and the status
  line says `poll-only mode`. This exists because sharing a Socket Mode app between two
  consumers is structurally broken — Slack load-balances each event to exactly one open
  connection and the pane acks what it receives, so a pane pointed at another service's app
  (e.g. a corporate relay bot) both misses most events *and* silently steals the ones it gets
  from that service. A malformed app token is still a loud error; only genuine absence selects
  poll-only mode. Bonus: the agent CLI (which never needed the app token) now works without one.

### Changed
- **A confirmed-lossy socket now escalates to fallback-cadence polling** (spec F17). The 0.1.9
  safety poll diagnosed a connected-but-undelivering socket but kept its 5-minute cadence —
  one 8-request batch per 5 minutes round-robins a typical subscription list every ~20
  minutes, a diagnosis rather than a usable feed. The first safety poll that finds
  socket-missed messages now switches to the ordinary `poll_fallback_secs` rhythm (default
  30s, jittered — the same request cost as a socket-down outage) until a live event actually
  arrives and proves the socket healed. Escalation and recovery both log to `slackr.log`.

## [0.1.9] — 2026-07-19

### Added
- **Silent-socket safety poll.** A Slack app with Socket Mode enabled but no `message.*` event
  subscriptions (easy to hit on a shared corporate app) connects successfully and then delivers
  nothing — and a healthy-looking socket suppressed all polling, freezing the feed forever after
  backfill. After 5 minutes with zero live events, the pane now spends one ordinary
  request-budgeted poll batch as a safety net; if that poll finds messages the socket should
  have delivered, the status line names the likely cause (`check the Slack app's event
  subscriptions`) and `slackr.log` records the count. Costs at most ~8 requests per 5 minutes,
  and only while the silence lasts. README documents the symptom under §Slack app setup.

## [0.1.8] — 2026-07-19

### Added
- **Delivery-diagnostic log trail** (`slackr.log`, complementing 0.1.7's gated-drop lines): a
  line per socket `connected`/`down` transition — a healthy socket was otherwise completely
  silent, so "was the connection even up when that message was sent?" was unanswerable — and a
  line per applied live DM/MPIM message (conversation id and `ts` only, never message text).
  Together with the drop trail, a "didn't see a DM" report now resolves from the log alone:
  arrived-and-applied, arrived-and-gated, or never arrived.

## [0.1.7] — 2026-07-19

### Fixed
- **Out-of-cap DMs were invisible in the two places a user looks for a DM.** A live message on
  a DM outside the `dm_limit`-subscribed set reached the Feed, but rendered as `#<raw id>` and
  never tripped the Mentions tab's every-DM-is-a-mention rule — mention detection and labeling
  consulted only the subscribed-conversation tables and defaulted to "channel". Conversation
  kind now resolves through the workspace snapshot (and Slack's `D` id prefix for a DM opened
  mid-session), so an unsubscribed DM badges in Mentions and reads as `@person`.
- **Live events dropped by the allow-list gate are now logged.** A gated-out message was
  indistinguishable from a delivery bug; `slackr.log` (when `HERDR_PLUGIN_STATE_DIR` is set,
  which herdr always does) now records each drop with the conversation id, its snapshot kind,
  and the `dms` flag.

### Changed
- **`r` now actually refreshes everything.** It previously polled a single 8-request round-robin
  batch (while the docs claimed a full re-pull); it now polls one batch immediately *and* arms
  the same paced, request-budgeted sweep the reconnect catch-up uses, so every subscribed
  conversation gets one watermarked fetch without bursting past the rate budget. The status line
  shows `refreshing n conversations`.

## [0.1.6] — 2026-07-18

### Changed
- **Threading display redesigned around how the feed is actually read.**
  - *Collapsed threads are one quiet, informative row.* The scattered `↳ @author replied: …`
    rows a busy thread sprayed through the Timeline are gone; the root's marker now carries the
    freshness itself — `↳ 3 replies · @alice: sounds good` — count first so it survives any pane
    width. The marker's arrival is the newest reply's, so the unread divider and the `↓ n new`
    counter still surface new thread activity exactly once, at the thread.
  - *Expanded replies sit on an aligned connector rail.* Replies swap the repeated channel label
    for muted `├─`/`└─` connectors padded to the root label's width, so author/time/text columns
    stay aligned and the thread reads as one visual block with an unmistakable end. The leftmost
    character now tells the row type at a glance (`#`/`@` message, `├`/`└` in-thread, `↳`
    collapsed summary).
  - *The Threads view is a real triage digest.* Each thread gets a bold header — reply count
    first, time column showing the thread's latest activity (the view's sort key) rather than
    the root's age — then its newest three replies on the rail, with one muted `… n earlier
    replies` line standing in for the rest (Enter on it, or anywhere in the thread, refetches
    the full thread).

### Added
- **`lookback_days` config key (default 7, `0..=365`, `0` = unlimited).** Bounds how far back any
  history fetch reaches — the *depth* companion to the request budget's *rate* cap. Startup
  backfill drops messages older than the horizon, and every incremental fetch (polling, the
  post-reconnect catch-up sweep, the DM scan) clamps its `oldest` to it, so a watermark left over
  from a weeks-long gap no longer sends pagination chasing history the 300-message retention cap
  would mostly discard anyway. Deliberately conservative for shared Slack apps, where the
  rate-limit pool is per app + workspace and this pane is not the only consumer.

### Changed
- **Catch-up sweep pacing relaxed from 10s to 15s between batches.** Worst sustained sweep rate
  drops from ~48 to ~32 requests/min, leaving real Tier-3 headroom for other consumers of a
  shared app key.
- **Poll-fallback and catch-up cadences now carry ±25% jitter** (the socket reconnect schedule
  already did). A Slack outage flips every pane on a shared app key into polling mode anchored to
  the same moment; fixed intervals kept their request batches in lockstep against the shared
  rate-limit pool indefinitely, jitter spreads the cohort out within a few cycles.
- **Conversation listing excludes archived channels** (`exclude_archived=true` on every
  `conversations.list` call). An older workspace accumulates thousands of dead channels that
  doubled the startup list's Tier-2 page count for rows a live feed can never subscribe to.
  Naming an archived channel in `channels` now fails resolution like any unknown name.

### Fixed
- **The `channels` allow-list now governs live delivery, not only fetching.** Socket Mode
  delivers events for every conversation the token can see, and the pane applied them all — so
  every joined channel in the workspace leaked into the Feed (with raw `#C…` id labels),
  regardless of config. Live `Message`/`Changed` events for channels/groups not named in
  `channels` are now dropped. DMs/MPIMs keep their documented always-arrive guarantee — including
  out-of-cap DMs and DMs first opened mid-session (admitted by Slack's `D` id prefix) — unless
  `dms = false`, which now suppresses live DM delivery the same way it suppresses subscription.
  If you relied on the firehose, name those channels in `channels`.

- **Poll batches now meter requests, not conversations.** History pagination made one
  conversation cost anywhere from one request (caught up) to ten (a large gap), so an
  8-conversation batch could issue up to ~80 requests right after a long outage — past Slack's
  Tier-3 budget at the exact moment a 429 was most likely, in the code that exists to recover
  from outages. `POLL_BATCH` is now a request budget shared by the poll tick and the
  post-reconnect catch-up sweep: a batch stops early once its spent requests reach the budget and
  the round-robin cursor rewinds to the first unvisited conversation, so big-gap sweeps
  automatically cover fewer conversations per tick instead of multiplying request volume. One
  conversation's own pagination may overshoot the budget it started under (bounded by the
  10-page cap) — accepted deliberately, since truncating a fetch mid-span would advance the
  watermark past unfetched messages, recreating the gap bug pagination exists to fix.

- **Messages arriving during a socket outage could be lost for good.** Slack Socket Mode never
  redelivers events that fired while the connection was down, and the polling fallback both waits
  out a grace period and round-robins only 8 conversations per tick — so a brief disconnect (or one
  shorter than a full round-robin sweep) silently dropped whatever landed in the gap. Every
  reconnect now arms a one-time catch-up sweep: each subscribed conversation gets one watermarked
  `conversations.history` fetch, paced in 8-conversation batches every 10 seconds. A conversation
  that missed nothing answers with an empty body, so the common post-blip sweep is nearly free.
- **A burst of more than 50 messages between polls left a permanent gap.** The incremental history
  fetch took only Slack's newest-first first page and then advanced its newest-seen watermark past
  everything else, so the older part of a large burst was never fetched again. Incremental fetches
  now follow Slack's pagination cursor (bounded at 10 pages per conversation per poll; hitting the
  bound is logged, not silent).
- **A rate limit mid-batch skipped the rest of that batch until the round-robin wrapped.** The poll
  cursor now rewinds to the conversation the 429 interrupted, so it and the batch's remainder are
  retried right after the cooldown instead of waiting a full cycle.
- **Unbounded memory and CPU growth over long sessions.** The message store never pruned, and every
  frame rebuilt row projections with a full store scan *per thread root* — quadratic work that grew
  for as long as the pane ran. Each conversation now retains its newest 300 messages (unread
  mentions are exempt from pruning until read), thread replies are resolved through a single-pass
  index instead of per-root scans, and the terminal only redraws when something actually changed (a
  socket event, poll result, keypress, resize, or the UTC day flipping under dated timestamps)
  rather than unconditionally every 250ms.
- **The 5-minute out-of-cap DM scan listed every conversation in the workspace.** It now requests
  `types=im,mpim` only, so it no longer pages through every public channel each interval — on a
  large workspace that was dozens of Tier-2 requests spent on rows the scan filtered out anyway.
- **Blank pane during a slow startup.** A rate-limited backfill can legitimately sleep up to 60
  seconds before its one retry; the pane now draws a "connecting to slack" frame before backfill
  starts instead of sitting empty, indistinguishable from a hang.

### Added
- **README: rate-limits note for newer Slack apps.** Non-Marketplace apps created after May 2025
  get roughly one `conversations.history`/`conversations.replies` request per minute with `limit`
  capped at 15; the note explains what that does to backfill and polling mode (Socket Mode is
  unaffected).

## [0.1.5] — 2026-07-13

### Fixed
- **`dm_limit` could leave a DM going silent in polling mode.** A message on a DM outside the
  `dm_limit` cap always showed up over a live Socket Mode connection, but in polling-fallback mode
  it had no path to the pane at all until that DM happened to rank back inside the cap. Polling
  mode now runs a dedicated out-of-cap DM activity scan every 5 minutes: it re-checks every DM
  outside the cap for new activity and fetches the single most-recently-active one that changed (at
  most one extra request per tick, regardless of how many changed at once — the rest wait for the
  next scan), so a new message in *any* DM reaches the pane in both delivery modes now, with a
  bounded worst-case delay in polling mode instead of indefinite silence. This scan is skipped
  outright during an active rate-limit cooldown, so it never adds pressure on top of a 429.

### Added
- **`dm_allow` config key.** Names DMs/MPIMs (Slack display names, matched exactly and
  case-insensitively) that are always subscribed and actively polled, regardless of `dm_limit` —
  for the handful of people you never want to fall out of the cap by inactivity. `dms = false`
  still suppresses them, same as any other DM.
- **Focus mode (`f`).** A third Feed-tab view, alongside Timeline and Threads: only messages that
  arrived live during the current pane session (nothing from startup backfill) *and* either came
  from an allow-listed DM (`dm_allow`) or hit a `focus_keywords` entry — a config key distinct from
  the existing Mentions-tab `keywords`. `t` (Threads) and `f` (Focus) are mutually exclusive views,
  each toggled by its own key rather than a shared cycle: pressing one while the other is active
  switches straight to it instead of bouncing through the Timeline first.
- **Dated timestamps.** A message from a UTC calendar day earlier than today now renders
  `Mon DD HH:MM` (e.g. `Jul 12 06:00`) instead of a bare `HH:MM` that could be mistaken for today.

## [0.1.4] — 2026-07-12

### Changed
- **Mentions and Threads now read top-to-bottom chronological.** Both used to list newest-first;
  they now match the Feed Timeline's direction — oldest at the top, newest at the bottom, like any
  chat client — so the whole pane scrolls the same way everywhere.
- **Real navigation keys.** `G`/`End` jumps to the newest row (the bottom), `g`/`Home` to the
  oldest (the top), `PageDown`/`PageUp` moves a full page, and `Ctrl-d`/`Ctrl-u` moves a half page —
  sized off the pane's actual on-screen row count, not a hardcoded guess.
- **`↓ n new` arrivals indicator.** Scrolled up when a message arrives, a muted `↓ n new` overlay
  now appears at the bottom-right of the row list, counting arrivals since you last left the
  bottom; it clears the moment you get back there, by any means. Already at the bottom, the view
  just follows new arrivals down automatically instead, exactly as before — the counter only ever
  appears once you're scrolled away from "now".
- **Thread expansion is discoverable from anywhere on the thread.** `Enter` used to expand/collapse
  a thread only from its collapsed `↳ n replies` marker row; it now works the same way from the
  thread's own root message, any of its nested replies once expanded, or a reply's activity row
  (below) while still collapsed — no more hunting for the exact marker row. Expanding/collapsing
  now also sets a one-line status confirming what happened (`thread expanded — n replies` /
  `thread collapsed`), and the footer shows an `enter expand/collapse thread` hint whenever the
  selected row would actually do something thread-related.
- **Reply activity rows.** A reply to a collapsed thread no longer just disappears into its root's
  `↳ n replies` count — it now also renders its own row at its actual chronological position in the
  Timeline (`↳ @author replied: <text>`), alongside the root's usual marker (which still shows the
  total). `Enter` on an activity row expands the thread it belongs to. Once a thread is expanded,
  its replies go back to nesting under the root and stop emitting activity rows.

### Fixed
- `expand_status`'s thread-expanded status line read `"thread expanded — 1 replies"` for the
  one-reply case; it now reads `"thread expanded — 1 reply"` (still `"n replies"` for every other
  count).

## [0.1.3] — 2026-07-12

### Added
- **Threads view.** `t` toggles the Feed tab (Timeline-only) between the normal chronological
  stream and a threads-only digest: every thread, newest activity first, root plus every
  locally-known reply always shown nested beneath it — no collapsed marker in this view. Non-
  threaded messages are excluded entirely. `Enter` on a row here always (re)fetches that thread's
  replies, rather than the Timeline's expand/collapse toggle. A reply whose root was never
  backfilled gets a synthetic `(thread — root not loaded)` header instead of vanishing; selecting
  it and hitting `Enter` fetches the real root over REST, which quietly replaces the placeholder
  on the next redraw — no separate "heal" action needed.
- **Colored row segments.** Every row now renders as separately styled spans instead of one flat
  color: the conversation label in the theme's accent (`lavender`), the author in `green`, the
  time and thread/divider markers in the muted `overlay1` tone, and the message text in the
  default foreground — consistent across every configured palette.
- **Thread metadata everywhere.** Messages now carry Slack's own `reply_count`, so a thread's
  displayed reply count is accurate immediately after backfill, before any reply has actually
  been fetched — previously it only reflected replies already stored locally.
- **Polling reply-refresh.** While in fallback polling mode, up to 2 of each tick's 8-request
  budget now rotate over "active" threads (currently expanded, or whose Slack-reported reply
  count outpaces what's stored locally), fetching just the replies newer than the newest one
  already known. The remaining slots still cover conversations as before; with no active threads,
  the full budget goes to conversations, so the reservation never goes to waste.

### Changed
- `refresh_thread`'s (Threads-view `Enter`) failure status now reads `thread refresh failed: …`,
  distinct from the Timeline's `replies failed: …`, so the status line names which action failed.

## [0.1.2] — 2026-07-12

### Added
- **`dm_limit` config key** (default 20, valid `0..=200`): caps how many DMs/MPIMs are
  actively subscribed when `dms = true`, keeping the most-recently-active ones. `0`
  subscribes none, even with `dms = true`. A DM outside the cap can still surface a
  message that arrives live over the socket; only backfill and polling respect the cap.
- **Shared `users.list` cache.** The workspace member directory (used for display names)
  is now cached on disk for 24h in `$HERDR_PLUGIN_STATE_DIR` (or the CLI's home-relative
  fallback), so the pane and every `mentions`/`feed` CLI invocation stop re-fetching the
  whole member list on every single run.

### Changed
- **Real `Retry-After` cooldowns.** A Slack rate limit now pauses the affected path for
  the server's actual advertised `Retry-After` seconds instead of a fixed 30-second
  guess, parsed from a `curl --write-out` trailer appended to every REST call.
- **Incremental, staggered polling.** The fallback poller no longer re-pulls every
  subscribed conversation's last 50 messages every tick: each tick visits at most 8
  conversations round-robin, and each conversation is asked only for messages newer
  than the last one already seen.
- **Startup backfill retries once on a rate limit** (sleeping the real `Retry-After`
  first) before giving up on the rest of that session's backfill list rather than
  failing pane startup outright; the socket/poll paths fill in the remainder.

### Fixed
- A socket reconnect (`Connected`) now also clears any pending rate-limit cooldown, not
  just the `polling`/status state — previously a cooldown set just before a reconnect
  could silently no-op the next manual poll (`r`) until its stale deadline lapsed.

## [0.1.1] — 2026-07-12

### Added
- **Agent CLI.** `herdr-slackr mentions [--json] [--limit <n>]` and
  `herdr-slackr feed [--channel "#name"] [--json] [--limit <n>]` give a coding agent its
  own read-only view of the feed — a fresh Slack REST scan per invocation, the same
  mention detection and history depth the pane uses. A mid-scan REST failure after at
  least one conversation already succeeded prints the partial rows collected so far plus
  a `slackr: partial results — …` note, rather than discarding them.
- **`skill-install` subcommand and manifest action.** `herdr-slackr skill-install`
  symlinks the bundled `herdr-slackr` skill into Claude Code's personal skills directory
  (`--copy` for a frozen copy, `--force` to replace, `--target` for anywhere else), and
  `--project` installs into the repo's universal `.agents/skills/` directory, which most
  skill-aware harnesses read. Idempotent; prints a CLAUDE.md snippet that makes agents
  check mentions proactively. `skill-path` prints the bundled skill's location. A
  `skill-install` action on the plugin manifest runs the same subcommand without needing
  the binary on `PATH`.
- **Post-install onboarding.** `install.sh` now ends with a next-steps block: the
  `npx skills add` one-liner (with the `skill-install`/plugin-action fallbacks), and
  where `tokens.toml`/`config.toml` live.
- **README "Working with agents" section** documenting the skill install paths, the
  CLAUDE.md proactive-check snippet, and `mentions`/`feed --json` usage.
- `specs/agent-cli.md` — the full CLI contract, including partial-results semantics.

## [0.1.0] — 2026-07-12

### Added
- **The pane.** A real-time Slack feed in a herdr sidebar: a `Feed` tab (one
  chronological stream across every subscribed channel and DM, threads collapsed under
  their root) and a `Mentions` tab (`@you`, every DM/MPIM, keyword hits, newest first,
  with a per-row read marker).
- **Socket Mode + polling fallback.** Live delivery over a Socket Mode WebSocket, with
  automatic reconnect/backoff; degrades to `conversations.history` polling when the
  socket can't run (a strict proxy, a network blip), so the pane stays live either way.
- **Config and tokens.** `config.toml` (`channels`, `dms`, `keywords`, `theme`,
  `poll_fallback_secs`) and `tokens.toml`/env-var token resolution
  (`SLACK_APP_TOKEN`/`SLACK_USER_TOKEN`), both fail-loud on an invalid file — no partial
  defaults. `chmod 600` enforced on `tokens.toml`.
- **Static musl Linux builds.** Prebuilt releases for `x86_64`/`aarch64` Linux link
  statically (musl), alongside macOS builds, downloaded by `install.sh` with checksum
  verification.
