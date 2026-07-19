---
Status: Current
Created: 2026-07-12
Last edited: 2026-07-12
---

# Configuration and tokens

How herdr-slackr validates and applies `$HERDR_PLUGIN_CONFIG_DIR/config.toml` and `tokens.toml`.

## Overview

The plugin config is one typed value, read fresh at pane startup. A valid file may set any subset of the supported keys; `channels` is the only one with no default.

```toml
channels = ["#eng-infra", "#releases"]   # required
dms = false                              # default true
keywords = ["deploy", "oncall"]          # default []
theme = "tokyo-night"                    # default "catppuccin"
poll_fallback_secs = 45                  # default 30
dm_limit = 15                            # default 20
dm_allow = ["alice", "Bob Smith"]        # default []
focus_keywords = ["incident", "p1"]      # default []
lookback_days = 14                       # default 7
```

| key                   | value                                                              |
| ---------------------- | ------------------------------------------------------------------- |
| `channels`            | required, non-empty array of `#`-prefixed channel names             |
| `dms`                 | boolean; whether IMs/MPIMs are subscribed alongside `channels`       |
| `keywords`            | array of strings; extra Mentions-tab triggers, matched case-insensitively as a substring |
| `theme`               | a palette name (see below); an unrecognized name is a runtime warning, not a config error |
| `poll_fallback_secs`  | integer in `5..=300`; seconds between polls while the socket is down |
| `dm_limit`            | integer in `0..=200`; caps how many `Im`/`Mpim` conversations are subscribed when `dms = true`, ranked by most-recently-active (`conversations.list`'s `updated`); `0` subscribes none even when `dms = true` |
| `dm_allow`            | array of non-empty strings (no other format restriction — free-form Slack display names, not `#`-prefixed); `Im`/`Mpim` counterpart names always subscribed regardless of `dm_limit`, matched exactly and case-insensitively against the conversation's resolved name; `dms = false` still suppresses them |
| `focus_keywords`      | array of strings; Focus-view triggers (see `pane.md`), matched case-insensitively as a substring — the same rule `keywords` uses, but a distinct list kept deliberately separate from it |
| `lookback_days`       | integer in `0..=365`; how far back any history fetch reaches — backfill drops messages older than the horizon, and every incremental fetch (polling, catch-up, DM scan) clamps its `oldest` to it; `0` means unlimited |

Tokens live in a separate file, `tokens.toml` in the same directory, or the environment:

| token       | env var             | `tokens.toml` key | required prefix | required? |
| ------------ | -------------------- | ------------------ | ----------------- | ---------- |
| app-level   | `SLACK_APP_TOKEN`    | `app_token`         | `xapp-`           | optional — genuine absence (no env, no key) selects **poll-only mode**: no socket worker, `polling` latched true, the fallback cadence is the permanent delivery path; a present-but-malformed value is still a loud error (absence is an opt-out, a typo is a mistake) |
| user OAuth  | `SLACK_USER_TOKEN`   | `user_token`        | `xoxp-`           | always     |

## Behavior

| #  | Always true                                                                                    |
| -- | -------------------------------------------------------------------------------------------------|
| C1 | A missing `config.toml` fails the same as one missing `channels` — `channels` has no default.    |
| C2 | An omitted key other than `channels` uses that key's default.                                     |
| C3 | An unknown key in `config.toml` makes the whole file invalid.                                     |
| C4 | An invalid value for any key in `config.toml` makes the whole file invalid.                        |
| C5 | An invalid `config.toml` applies none of its keys — the pane renders the config error, nothing else. |
| C6 | `theme` is validated separately from the rest of `config.toml`: an unrecognized name does not fail the file: the pane starts on the default palette with a one-line status warning naming the bad value. |
| C7 | Per token, resolution is env first, then `tokens.toml`; a present env value always wins even when the file also has an entry. |
| C8 | A present token failing its expected prefix (`xapp-`/`xoxp-`) is a loud error, regardless of source. |
| C9 | On Unix, a `tokens.toml` readable by group or world is refused with a `chmod 600 <path>` remedy, before its contents are ever parsed. |
| C10 | No error message constructed by config or token resolution ever contains a candidate token value. |
| C11 | A configured channel name that resolves to no real conversation is an error naming that channel. |
| C12 | When the subscribed DM/MPIM set exceeds `dm_limit`, the cap keeps the most-recently-active ones (`updated` descending); if any candidate is missing `updated`, ranking falls back to Slack's own list order instead of guessing, logged once. `dm_limit = 0` excludes DMs entirely, independent of `dms`. A DM outside the cap is not backfilled or polled by the regular per-tick round-robin, but a message on it can still arrive live over the socket, and polling mode has its own out-of-cap activity scan (`slack-host.md` F12) — `dm_limit` bounds active-subscription *count*, never new-arrival *delivery*. |
| C13 | A DM/MPIM whose resolved name exactly matches (case-insensitively, no substring matching) a `dm_allow` entry is always included in the subscribed set, never subject to the `dm_limit` cut — the cap applies only to the remaining non-allow-listed pool. `dms = false` suppresses allow-listed DMs too; an explicit "no DMs" wins over the allow-list. |
| C14 | `focus_keywords` is validated and matched exactly like `keywords` (array of strings, case-insensitive substring), but is a wholly separate list consulted only by the Focus view (`pane.md`) — setting one never changes what the other matches. |
| C15 | `lookback_days` bounds fetch *depth*, never live *delivery*: a socket event upserts regardless of age, and the horizon is applied at fetch boundaries only (backfill filters client-side; incremental paths clamp `oldest` server-side, inclusive at the boundary). `0` disables the horizon entirely, restoring watermark-only behavior. |

An error names the config path and the read, syntax, key, or value failure, and states the expected form when a value is invalid.

| entrypoint    | invalid config/token outcome                                    |
| -------------- | ------------------------------------------------------------------|
| pane (startup) | the pane's only screen is the remedy message; no socket, no REST, no crash (→ O4 in `overview.md`) |

The pane reads config and tokens once, at startup, from one process launch. There is no live-reload: a fixed `config.toml` requires relaunching the pane (unlike reviewr's per-refresh reread — herdr-slackr's socket worker and message store are long-lived for the pane's whole session, not rebuilt per poll tick).

## Failure semantics

- A missing `config.toml` and a missing `tokens.toml` are indistinguishable in kind from any other invalid-input case: both are a fail-loud remedy screen, never a silent default (`channels` has none; a token file has no acceptable absence once the matching env var is also unset).
- A missing `tokens.toml` is not a config-file error — it is a `TokenError` naming both the unset env var and the file that doesn't exist, with the exact `key = "prefix..."` line to add.
- `theme`'s failure mode is the one deliberate exception to the fail-loud config contract (C6): the pane's job is to stay legible, and a cosmetic default is a safer failure than blocking the whole feed over a typo'd palette name.

## Related specs

- [overview](./overview.md)
- [pane](./pane.md)
- [slack-host](./slack-host.md)
