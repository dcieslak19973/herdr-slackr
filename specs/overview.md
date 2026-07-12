---
Status: Current
Created: 2026-07-12
Last edited: 2026-07-12
---

# herdr-slackr

A real-time Slack feed pane in a herdr pane: a live stream of subscribed channels and DMs, and a mentions/triage view, so the user never alt-tabs to Slack to know whether something needs them.

## Overview

One binary (`herdr-slackr`, Rust + ratatui) runs in a herdr pane. It renders in the real terminal, so fonts and colors are whatever the user already runs.

The user's loop:

```
open the pane → glance at the Feed or Mentions tab → context-switch to Slack if something needs them, keep working if not
```

Two tabs:

| tab        | shows                                                                          |
| ---------- | -------------------------------------------------------------------------------|
| `Feed`     | one chronological stream across every subscribed conversation                  |
| `Mentions` | only what triggers attention — `@you`, any DM/MPIM, keyword hits — newest first |

## Scope

- Real-time delivery via Slack Socket Mode (`slack-host.md`), degrading to REST polling when the socket can't run.
- The Feed tab: chronological cross-conversation ordering, inline thread collapse/expand, an unread divider (`pane.md`).
- The Mentions tab: `@you` detection, DM/MPIM-as-mention, keyword hits, per-row read state (`pane.md`).
- Slack text-entity resolution for display (`<@U…>`, `<#C…>`, links) — plain text, no mrkdwn styling.
- One permalink-open action (`o`) into the browser.
- Config and token resolution from `$HERDR_PLUGIN_CONFIG_DIR` (`config.md`).

## Non-goals

Out of scope by explicit choice, not by oversight:

- Posting, replying, or reacting in Slack. A separate Slack MCP covers agent-side read/reply; humans reply in Slack itself.
- Message persistence across pane restarts.
- Native herdr nav/badge integration beyond an unverified terminal-title spike (see `pane.md`).
- Multi-workspace / Enterprise Grid support — one workspace, one token pair.
- Message search or a searchable archive — the Feed tab is a live stream only.

## Invariants

| #  | Always true                                                                                                        |
| -- | -------------------------------------------------------------------------------------------------------------------|
| O1 | The pane never posts, reacts, edits, deletes, or marks anything read in Slack. Every Slack-facing call is read-only (see `slack-host.md`'s method table). |
| O2 | A Slack token (`xapp-…`/`xoxp-…`) is never written to argv, a log line, an error message, or the pane's own display, in any form. |
| O3 | An unknown `config.toml` key or an invalid value for any key blocks the whole file — no partial-default fallback (see `config.md`). |
| O4 | A missing or invalid token, or a config failure, renders the pane's full-tab remedy screen; it never crashes the process. |
| O5 | The crate forbids `unsafe`.                                                                                         |

## Related specs

- [config](./config.md)
- [pane](./pane.md)
- [slack-host](./slack-host.md)
- [agent-cli](./agent-cli.md)
