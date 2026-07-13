# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
