# Agent CLI: read-only feed access, skill, post-install onboarding

**Date:** 2026-07-12
**Status:** Approved
**Repo:** `dcieslak19973/herdr-slackr`, branch `agent-cli`
**Extends:** `2026-07-12-herdr-slackr-design.md` (the pane spec; its invariants hold)

## Goal

Give coding agents read-only access to the same Slack view the pane shows — triage
mentions, pull channel context — through subcommands on the existing binary, taught by an
installable skill (parity with herdr-reviewr's), and tell users how to install that skill
the moment the plugin lands.

## CLI

With no subcommand the binary launches the pane as today. New:

```
herdr-slackr mentions [--json] [--limit <n>]
herdr-slackr feed [--channel "#name"] [--json] [--limit <n>]
herdr-slackr skill-path
herdr-slackr skill-install [--target <dir> | --project] [--copy] [--force]
```

- **Data source:** fresh Slack REST reads per invocation (the pane's memory is another
  process). Same user token, same read-only methods (`auth.test`, `conversations.list`,
  `conversations.history`, `users.list`), same rate-limit handling (surface, no retry).
- **`mentions`:** scan the configured conversations' recent history (the pane's backfill
  depth, 50/conversation) and print rows where `entities::is_mention` fires — @you, any
  DM/MPIM, keyword hits — newest first, capped by `--limit` (default 20). Human rows:
  `#chan  @author  HH:MM  text` (entities resolved); `--json` emits the raw message
  documents plus conversation names.
- **`feed`:** the same scan without the mention filter; `--channel` restricts to one
  configured conversation (error naming it if not in the config).
- **Config/token discovery outside herdr:** `$HERDR_PLUGIN_CONFIG_DIR` when set, else
  `~/.config/herdr/plugins/config/dcieslak19973.slackr/` (herdr's standard layout). Both
  named in the error when neither yields a config.
- **`skill-path` / `skill-install`:** ported from herdr-reviewr's cli.rs contract
  verbatim (symlink default, copy fallback + `--copy`, idempotency, `--force`, `--project`
  → `.agents/skills/herdr-slackr/`, hint block on success), pointing at
  `skills/herdr-slackr/SKILL.md`, skill name `herdr-slackr`.
- Errors and exit codes follow reviewr's CLI conventions (usage → 2, one-line
  `slackr: …` stderr → 1). Tokens never in argv/output (the existing rules).

## Skill

`skills/herdr-slackr/SKILL.md` (spec-standard frontmatter; description triggers on
"check my Slack mentions", "what's happening in #channel", slackr setup/troubleshooting):

- Triage: `herdr-slackr mentions --json` — what needs the user's attention.
- Context: `herdr-slackr feed --channel "#x"` — recent discussion when the user references
  a channel.
- READ-ONLY rule: this tool never posts; reply via the user's Slack MCP or tell the user.
- Binary discovery: PATH (linked at install), else `$HERDR_PLUGIN_ROOT/bin/herdr-slackr`,
  else ask.
- Setup/troubleshoot: tokens.toml/env + 0600, config keys, the degraded states
  ("socket unavailable — polling", token remedies), the log at
  `$HERDR_PLUGIN_STATE_DIR/slackr.log`.
- Pane control: `herdr plugin action invoke toggle|open|close --plugin dcieslak19973.slackr`.

## Post-install onboarding (both repos)

`install.sh` ends with a printed next-steps block after a successful install:

1. Install the agent skill — the npx one-liner, `<binary> skill-install`, or the
   PATH-free `herdr plugin action invoke skill-install --plugin <id>`.
2. (slackr only) Where tokens and config go, one line each.

herdr-reviewr gets the same epilogue (script-only change; no release needed there).
A `skill-install` action is added to slackr's manifest (reviewr already has one).

## Non-goals

- No posting/reacting (invariant kept); no history beyond the backfill depth; no pane-CLI
  IPC; no caching between CLI invocations.

## Testing

- Mention/feed row selection + formatting: pure fns over fixture messages (reuse the
  existing fixture style); config-dir fallback: unit test with env injection.
- CLI integration tests (spawn binary): config-missing error names both locations;
  usage/exit codes; skill-path/skill-install ported tests (adapt reviewr's).
- Live behavior (real Slack) stays on the README smoke checklist.
