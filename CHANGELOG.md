# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
