---
Status: Current
Created: 2026-07-12
Last edited: 2026-07-12
---

# Slack host

Socket Mode lifecycle, the ack contract, URL rotation, polling fallback, and the REST methods this pane calls, with their scopes.

## Overview

Two channels to Slack, both read-only:

| channel        | transport                          | carries                                                        |
| --------------- | ------------------------------------ | ----------------------------------------------------------------|
| Socket Mode    | `tungstenite` + `rustls` WebSocket   | live message/change/delete events, on their own thread          |
| REST (Web API) | `curl`, shelled out                  | backfill, thread replies, user/conversation lookups, permalinks |

Every REST call runs `curl --silent --show-error --config -`, the bearer token on stdin, never argv. Slack answers HTTP 200 even on a Slack-level failure (`{"ok": false, "error": "…"}`), so every response is parsed and its `ok` field checked regardless of transport outcome; `--fail` is deliberately absent since it would only catch an HTTP-status failure Slack itself rarely produces this way.

**REST methods used:**

| method                      | token   | scope(s) needed                              | purpose                             |
| ---------------------------- | ------- | ----------------------------------------------- | --------------------------------------|
| `apps.connections.open`     | app     | `connections:open`                              | open a Socket Mode WebSocket URL     |
| `conversations.list`        | user    | `channels:read`, `groups:read`, `im:read`, `mpim:read` | resolve subscribed conversations |
| `conversations.history`     | user    | `channels:history`, `groups:history`, `im:history`, `mpim:history` | backfill / poll fallback |
| `conversations.replies`     | user    | same `*:history` scopes as above                | fetch a thread's replies on expand   |
| `users.list`                | user    | `users:read`                                    | display-name cache                   |
| `chat.getPermalink`         | user    | (covered by the `*:read` scopes above)          | the `o` key's browser-open target    |
| `auth.test`                 | user    | none beyond authentication                       | resolve the self user id for mention detection |

Every method above is a read. Nothing in this pane calls a `chat.postMessage`/`reactions.add`/`conversations.mark`-shaped endpoint (O1 in `overview.md`).

## Behavior

**Socket lifecycle:**

| #  | Always true                                                                                             |
| -- | ----------------------------------------------------------------------------------------------------------|
| S1 | Every connection attempt — including the first — calls `apps.connections.open` fresh; a Socket Mode URL is single-use and is never reused after a `disconnect` or a dropped connection. |
| S2 | A `hello` frame is the only event with no `envelope_id`; it yields `Connected` and resets the reconnect backoff to 0. |
| S3 | Every other frame that carries an `envelope_id` is acked (`{"envelope_id":"<id>"}`, written back over the same socket) exactly once, regardless of whether it mapped to zero or more app events. |
| S4 | A `disconnect` frame ends the connection without an ack (there is nothing to acknowledge) and triggers a reconnect via S1. |
| S5 | An unparseable frame with no recoverable `envelope_id` is dropped silently; one with an `envelope_id` is still acked, so Slack does not redeliver it forever. |
| S6 | The reconnect backoff is `1, 2, 4, 8, …` seconds, capped at 60, jittered ±25%, reset to 0 by the next successful `hello`. |
| S7 | The underlying TCP read has a 30s timeout; a timeout firing is treated as silence, not connection death — Slack pings a healthy socket far more often than that. |
| S8 | If read-timeout ticks pile up for more than a 90s liveness deadline (three read timeouts) since the last frame actually read, the silence is presumed to be a dead connection (e.g. a firewall dropping packets with no FIN/RST) and the loop errors out so the `Down` + reconnect/backoff path engages — otherwise a dead-air network would never trip the polling fallback. |

**Ack contract (event → emitted app events → ack):**

| `events_api` event subtype              | app event(s)                          | acked? |
| ----------------------------------------- | ---------------------------------------- | -------- |
| absent, or `bot_message`, with a `ts`    | `Message`                                | yes    |
| `message_changed`, nested `message.ts` present | `Changed` (built from the nested `message`) | yes |
| `message_deleted`, `deleted_ts` present  | `Deleted { conv, ts }`                    | yes    |
| any of the above, missing its required field | none                                   | yes — still acked so Slack does not redeliver |
| any other subtype                        | none                                     | yes    |

**Polling fallback:**

| #   | Always true                                                                                       |
| --- | ---------------------------------------------------------------------------------------------------|
| F1  | The app flips into polling mode the instant a `Down` event arrives — before any backoff cycle completes. |
| F2  | The pane's `poll_tick` (a `conversations.history` re-fetch per subscribed conversation) does not start firing until the current down streak has outlived one full backoff cycle (the worker's own first retry interval), so a fast reconnect is not raced by an eager poll. |
| F3  | Once polling has started, it repeats every `poll_fallback_secs` until a `Connected` event clears the streak. |
| F4  | A message seen via both the socket and a poll re-fetch appears exactly once — `(conv, ts)` identity dedups. |
| F5  | A poll-delivered edit (re-fetched history carrying a since-edited message) still surfaces the edit, without moving the row's position in the unread divider's arrival order. |
| F6  | Socket recovery (`Connected`) clears `polling` and the status-bar notice silently — no separate "back online" message. |

## Failure semantics

- `invalid_auth` / `token_revoked` from any REST call surfaces as the pane's `Blocked` remedy (`pane.md`), naming Slack's error string verbatim.
- A `ratelimited` response (Slack's signal for HTTP 429) maps to a fixed 30-second backoff — Slack does not echo a `Retry-After` value into the JSON body, so there is nothing more precise to parse.
- A channel the configured user cannot read fails `App::build` with a error naming the channel; this reaches the pane as `Blocked`, not a per-row skip.
- Any curl transport failure (DNS, TLS, refused connection, cancelled fetch) is classified `Other` and surfaces as the calling operation's own failure path — a backfill/poll failure is swallowed per-conversation (silently skipped, not fatal to the whole pane), while a startup-critical call (`conversations.list`, `auth.test`, `users.list`) fails the whole `App::build`.
- An unknown/unparseable socket envelope is never a crash: it is acked-and-skipped (S5) with one log line, written to `$HERDR_PLUGIN_STATE_DIR/slackr.log` when that directory is set, a no-op otherwise.
- A socket gone silent without a read error (dead air — e.g. a firewall dropping packets rather than resetting the connection) is not treated as live forever: past a 90s liveness deadline (S8) since the last frame actually read, the loop errors out so `Down` fires and the polling fallback can engage.

## Related specs

- [overview](./overview.md)
- [config](./config.md)
- [pane](./pane.md)
