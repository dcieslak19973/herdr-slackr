---
Status: Current
Created: 2026-07-12
Last edited: 2026-07-12
---

# Agent CLI

The `mentions`/`feed`/`skill-path`/`skill-install` subcommands: read-only Slack access for a coding agent, config/token discovery outside herdr, and the installable skill that teaches an agent to use them.

## Overview

With no subcommand the binary launches the pane (`pane.md`) as always. Four more subcommands live on the same binary, each a short-lived, one-shot process rather than the pane's long-running loop:

```
herdr-slackr mentions [--json] [--limit <n>]
herdr-slackr feed [--channel "#name"] [--json] [--limit <n>]
herdr-slackr skill-path
herdr-slackr skill-install [--target <dir> | --project] [--copy] [--force]
```

`mentions` and `feed` open a fresh Slack REST session on every invocation — the pane's in-memory message store belongs to another process and is never read or shared. Same user token, same read methods the pane's own backfill uses (`auth.test`, `conversations.list`, `conversations.history`, `users.list`), same rate-limit handling (surface the remedy, no retry). `skill-path` prints the bundled skill's location; `skill-install` copies or symlinks it into an agent's skills directory. Both are ported from herdr-reviewr's `cli.rs` contract verbatim, adapted to this crate's skill name and directory.

## Behavior

**`mentions` / `feed`:**

| #  | Always true                                                                                                          |
| -- | -----------------------------------------------------------------------------------------------------------------------|
| A1 | `mentions` scans every configured conversation's recent history (the pane's backfill depth, 50/conversation) and prints the rows where `entities::is_mention` fires — `@you`, any DM/MPIM message, a keyword hit — newest first, capped by `--limit` (default 20). |
| A2 | `feed` runs the same scan without the mention filter, newest first, same `--limit` default and cap.                     |
| A3 | `feed --channel "#name"` restricts the scan to that one configured conversation; a name not in `config.toml` is an error naming it and listing what is configured. |
| A4 | Human output (the default) renders one row as `#chan  @author  HH:MM  text` (`@name` in place of `#chan` for a DM), entities resolved (`<@U…>`, `<#C…>`) the same way the pane renders them. |
| A5 | `--json` emits an array of message documents (`conversation`, `conv_id`, `author`, `author_id`, `ts`, `text`, `text_raw`) instead of the human rows, for a caller to parse. |
| A6 | `--limit <n>` must be a positive integer; `0`, a non-numeric value, or a missing value is a usage error (exit 2), not silently clamped. |
| A7 | Config/token discovery outside herdr checks `$HERDR_PLUGIN_CONFIG_DIR` first, else `~/.config/herdr/plugins/config/dcieslak19973.slackr/`; neither yielding a valid `config.toml`/token is an error naming both candidate locations. |

**Partial results:**

| #  | Always true                                                                                                          |
| -- | -----------------------------------------------------------------------------------------------------------------------|
| A8 | A scan fetches each selected conversation's history in order. If a fetch fails after at least one earlier conversation already succeeded, the scan stops there: it prints the rows already collected as usual (exit 0), then one `slackr: partial results — <reason> after n/total conversations` stderr note. |
| A9 | If the very first conversation's fetch fails, there is nothing to keep — that is a hard failure like any other REST error: no stdout, one `slackr: <reason>` stderr line, exit 1. |
| A10 | `<reason>` is the same classification `rest_fail` uses for a hard failure: the rate-limit remedy (`slack rate limit — retry in <n>s`), Slack's own error name, `curl not found on PATH`, or the classified transport detail — never raw debug output. |

**`skill-path` / `skill-install`:**

| #   | Always true                                                                                                          |
| --- | -----------------------------------------------------------------------------------------------------------------------|
| A11 | `skill-path` resolves and prints `skills/herdr-slackr/SKILL.md` under the installed plugin root (`<exe-dir>/../skills/…`), falling back to the cwd-relative dev-checkout path when the installed layout isn't found. |
| A12 | `skill-install` installs to `~/.claude/skills/herdr-slackr` by default, `--project` installs to `./.agents/skills/herdr-slackr` instead (read by Gemini CLI, GitHub Copilot, OpenCode, Amp, and others besides Claude Code), and `--target <dir>` installs anywhere else. `--project` and `--target` are mutually exclusive (usage error). |
| A13 | The install symlinks the bundled `SKILL.md` by default; `--copy` (or an unconditional fallback on Windows, or whenever symlink creation fails for any reason) installs a real file instead, with a one-line stderr note when it wasn't requested. |
| A14 | Re-running is idempotent: an install target that already resolves to the same bundled skill prints `already installed at <path>` and exits 0, without touching the filesystem. A conflicting file exits 1 naming it; `--force` replaces it. |
| A15 | A successful fresh install (or a `--force` replace) prints the target path plus a copy-pasteable `CLAUDE.md` snippet reminding the user that the skill list alone doesn't make an agent check Slack unprompted. |

## Failure semantics

- Every failure this module returns is exit 1 with exactly one `slackr: …` stderr line (`fail`/`rest_fail`) and no stdout; a usage error (bad flag, missing flag value, `--limit 0`, `--project` with `--target`) is exit 2 with the usage block on stderr, printing nothing else.
- A Slack token is never written to argv, an error line, or either output mode, in any form — the same invariant the pane holds (O2 in `overview.md`); a rate limit or auth failure surfaces Slack's classification, never a raw token or raw HTTP body.
- `skill-install`'s target-resolution and idempotency failures name the exact path involved (`cannot create <dir>: …`, `<dest> already exists and differs…`) so a scripted retry has something to act on without re-deriving the path itself.
- Nothing in this module ever calls a Slack write method (`chat.postMessage`, `reactions.add`, `conversations.mark`, …) — same invariant as the pane (O1 in `overview.md`); an agent that needs to reply does so through the user's own Slack MCP or by asking the user, never through this CLI.

## Related specs

- [overview](./overview.md)
- [config](./config.md)
- [slack-host](./slack-host.md)
