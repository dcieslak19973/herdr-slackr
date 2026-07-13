---
name: herdr-slackr
description: Check Slack mentions and pull recent channel context via herdr-slackr. Use when the user asks "check my Slack mentions", "what's happening in #channel", or asks to set up or troubleshoot herdr-slackr.
---

# herdr-slackr

The slackr sidebar and you read the same Slack view — its config's channels, plus DMs when
enabled. Every command here is a fresh, read-only Slack API call; nothing is cached between
invocations. Find the binary as `herdr-slackr` on PATH (the plugin install links it there);
if not found, use `$HERDR_PLUGIN_ROOT/bin/herdr-slackr` when that env var is set; otherwise
ask the user for the plugin root.

## Triage: what needs attention

    herdr-slackr mentions --json     # full documents: conversation, author, ts, text, text_raw
    herdr-slackr mentions            # human rows: #chan  @author  HH:MM  text

Returns messages that would land on the pane's Mentions tab: a literal `@you`, any DM/MPIM
message, or a configured keyword hit — in the CLI's own newest-first order (the pane's tab
reads oldest-to-newest), capped at 20 unless `--limit <n>` says
otherwise. Prefer `--json` when you're about to reason over the results (e.g. summarizing or
deciding what to reply to); the raw, unresolved text is under `text_raw` if you need it.

## Context: what's going on in a channel

    herdr-slackr feed --channel "#eng-infra" --json
    herdr-slackr feed                # every configured channel/DM, no mention filter

`--channel` must name one of the configured channels; the error lists the configured set if
it doesn't match. Use this when the user references a channel or asks "what did I miss in
#x" rather than a specific mention.

## Read-only — this tool never posts

`herdr-slackr` has no write path: no `chat.postMessage`, no reactions, nothing. If the user
wants to reply or take an action in Slack, do it through their Slack MCP server (if
configured) or tell them what to say so they can send it themselves. Do not attempt to shell
out to `curl` or any other tool to post on their behalf — that would defeat the point of a
read-only integration.

## Setup / troubleshooting

- Tokens: `$HERDR_PLUGIN_CONFIG_DIR/tokens.toml` (must be `chmod 600`) with `app_token =
  "xapp-..."` and `user_token = "xoxp-..."`, or `SLACK_APP_TOKEN`/`SLACK_USER_TOKEN` in the
  environment (env wins). A wrong-prefix or over-permissioned file is a loud error naming the
  remedy — never guess at fixing a token value yourself, tell the user what the error says.
- Config: `$HERDR_PLUGIN_CONFIG_DIR/config.toml` needs a non-empty `channels = ["#..."]`
  array; `dms`, `keywords`, `theme`, and `poll_fallback_secs` all have defaults.
- Degraded states: the pane's status line may show `socket unavailable (...) — polling` —
  that's expected fallback behavior, not a bug; it keeps working over REST polling. A blocked
  pane screen means config or tokens failed to resolve — the message on screen names the
  fix.
- Logs: `$HERDR_PLUGIN_STATE_DIR/slackr.log` when that directory is set (skipped socket
  frames, other diagnostics); unset is a silent no-op, not an error.
- Install this skill: `herdr-slackr skill-install` (symlinks by default; `--copy` to force a
  real file, `--project` to install into `.agents/skills/herdr-slackr` instead of
  `~/.claude/skills`, `--force` to replace a conflicting existing file). `herdr-slackr
  skill-path` prints the source `SKILL.md` this installs from.

## Pane control

    herdr plugin action invoke toggle --plugin dcieslak19973.slackr
    herdr plugin action invoke open --plugin dcieslak19973.slackr
    herdr plugin action invoke close --plugin dcieslak19973.slackr

Use these when the user wants the sidebar itself shown/hidden rather than a one-off read.
