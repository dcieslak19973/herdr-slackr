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
| F2  | The pane's `poll_tick` (an incremental, batched `conversations.history` scan — F7/F8) does not start firing until the current down streak has outlived one full backoff cycle (the worker's own first retry interval), so a fast reconnect is not raced by an eager poll. |
| F3  | Once polling has started, it repeats every `poll_fallback_secs` until a `Connected` event clears the streak. |
| F4  | A message seen via both the socket and a poll re-fetch appears exactly once — `(conv, ts)` identity dedups. |
| F5  | A poll-delivered edit (re-fetched history carrying a since-edited message) still surfaces the edit, without moving the row's position in the unread divider's arrival order. |
| F6  | Socket recovery (`Connected`) clears `polling`, the status-bar notice, and any pending rate-limit cooldown silently — no separate "back online" message. A cooldown started before the reconnect does not outlive it; a healthy socket means Slack accepted the connection, so the next poll (including a manual `r`) runs immediately rather than waiting out a now-stale deadline. |
| F7  | A tick visits at most 8 subscribed conversations (`POLL_BATCH`), round-robin from where the previous tick left off, wrapping back to the first conversation once every one has had a turn — request count is what Slack's limits meter, so ticking every subscribed conversation every time is what triggers them in the first place. |
| F8  | Each conversation's `conversations.history` call passes `oldest` as the newest `ts` already seen for that conversation (`None` on its very first poll), so a caught-up tick's response is typically empty instead of re-shipping the same page of messages every time. |
| F9  | A `RateLimited(secs)` hit during a tick sets a rate-limit status naming `secs`, starts a cooldown lasting exactly `secs`, and stops the rest of that tick's batch immediately (Slack's own signal to back off now). While the cooldown is unexpired, `poll_tick` skips entirely — no `conversations.history` call at all, not even a shortened batch — until `Connected` clears it early (F6) or the deadline passes on its own. |
| F10 | Startup backfill (`App::build`) retries a `RateLimited` conversation exactly once, sleeping the real `Retry-After` (capped at 60s) first. A second consecutive `RateLimited` on the retry stops backfilling the rest of the subscribed list (a status notice names where it stopped) without failing `build` — the socket/poll paths fill in the remainder once they're running. Any other error on the retry still fails `build` loud, naming the channel, same as an unretried failure. |
| F11 | A tick's 8-request budget (F7) is split, not additive: up to 2 of the 8 slots rotate round-robin over "active" threads first — an active thread is one currently expanded in the Timeline, or whose root's Slack-reported `reply_count` exceeds the number of replies stored locally for it — fetching each one's `conversations.replies` with `oldest` set to the newest reply `ts` already known for that thread (`None` on a thread's first-ever fetch). The remaining slots (6 when 2+ threads are active, all 8 when none are) go to conversations exactly as F7/F8 describe. A `RateLimited` hit during the thread slots sets the cooldown and skips the conversation slots entirely for that tick, same as a mid-batch hit during F7's own loop. |
| F12 | **Out-of-cap DM activity scan**, additive to (not part of) the `POLL_BATCH` budget above: at most once per 5-minute interval (`DM_SCAN_INTERVAL`, gated by `next_dm_scan`), and only on a tick where the `POLL_BATCH` conv/thread budget did *not* just hit `RateLimited` (a rate-limit hit stops the rest of that tick, this scan included — same "back off now" contract as F9). Not yet due costs nothing — not even a `conversations.list` call. When due: re-fetches the DM/MPIM conversation list via `conversations.list` (`types=im,mpim` — the scan never selects any other kind, so paging the workspace's channels would be pure waste; archived conversations are excluded on every list call) and diffs it against the previous snapshot (`App::all_conversations`, from `build` or the prior scan) to find every `Im`/`Mpim` conversation *outside* the actively-subscribed set whose Slack-reported `updated` moved past what was last observed for it (`App::dm_last_seen`'s watermark if the scan has fetched it before, else the prior snapshot's own `updated`, else `0` on a DM never seen before this scan). |
| F13 | Among however many out-of-cap DMs moved (F12), at most **one** gets an actual `conversations.history` call this tick — the single one whose `updated` moved the furthest (ties broken by conversation id) — bounding this scan to exactly one extra REST call per tick regardless of how many DMs changed simultaneously. Any others wait for the next 5-minute scan to pick them up in turn; nothing is lost, only deferred. The chosen DM's `history` call passes `oldest` as its last-scanned watermark (converted from the millisecond `updated` stamp to a synthetic-but-correctly-ordered `ts`), or `None` on that DM's first-ever scan fetch. Results fold in through the same `upsert_new` path as every other arrival, so Mentions and Focus qualification (`pane.md` FC1/FC2) apply automatically. |
| F14 | A `list_conversations` or `history` failure inside this scan sets `status` (and, on `RateLimited`, the cooldown) exactly like any other REST failure in this module (F9) — it surfaces, never silently swallows — but never fails the tick itself; the regular `POLL_BATCH` round-robin above already completed by the time this scan runs. |
| F15 | **Live-event admission**: a socket `Message`/`Changed` event is applied only when its conversation passes `App::admits_live` — subscribed conversations always; with `dms = true`, any `Im`/`Mpim` in the conversation-list snapshot regardless of subscription (the F12/F13 arrival guarantee's live half), plus any conversation absent from the snapshot whose id carries Slack's `D` DM prefix (a DM opened after startup); everything else — any channel/group the `channels` allow-list never named — is dropped. With `dms = false`, only subscribed conversations pass. `Deleted` is never gated (removing an unadmitted message is a no-op). The allow-list governs live delivery exactly as it governs fetching. |
| F16 | **Silent-socket safety poll**: while the socket is nominally healthy (`polling == false`), if no live `Message`/`Changed`/`Deleted` event has arrived for `SILENT_SOCKET_POLL` (5 minutes) and no poll ran in that window, the event loop spends one ordinary `poll_tick` batch (cooldown-gated and request-budgeted like any other). If that poll applies messages the socket should have delivered, the status line names the likely cause (missing `message.*` event subscriptions on the Slack app) and the log records the count — a connected-but-undelivering socket must degrade to polling plus a visible diagnosis, never a silently frozen feed. Both the silence trigger and a positive find write `slackr.log` lines. |
| F17 | **Lossy-socket escalation**: the first safety poll that finds socket-missed messages marks the socket *lossy*, and subsequent safety polls run at the ordinary `poll_fallback_secs` cadence (jittered, same request cost as a socket-down outage) instead of the 5-minute diagnostic cadence — one 8-request batch per 5 minutes round-robins a typical subscription list every ~20 minutes, which is a diagnosis, not a usable feed. The lossy flag clears the moment any live socket event arrives (the socket proved itself again), returning to the slow diagnostic cadence; it is never persisted across pane restarts. Escalation and de-escalation each write a `slackr.log` line. |

Net effect of F12–F14 together with the Socket Mode path (which already delivers every live event workspace-wide, capped set or not): **`dm_limit` bounds which DMs are actively, continuously polled — it never bounds whether a new message in *any* DM eventually reaches the pane**, in either delivery mode. The worst case for an out-of-cap DM in polling mode is up to a 5-minute detection delay, not indefinite silence.

## Failure semantics

- `invalid_auth` / `token_revoked` from any REST call surfaces as the pane's `Blocked` remedy (`pane.md`), naming Slack's error string verbatim.
- A `ratelimited` response (Slack's signal for HTTP 429, either in the JSON body's `error` field or the transport-level HTTP status) maps to `RestError::RateLimited(secs)`. Slack does not echo `Retry-After` into the JSON body itself, so every REST call appends a `--write-out` trailer capturing the response's real HTTP status and `Retry-After` header; `secs` is that header's value when present, else a 30-second default (no header sent, or an older `curl` that predates `--write-out`'s `%header{}` support).
- A channel the configured user cannot read fails `App::build` with a error naming the channel; this reaches the pane as `Blocked`, not a per-row skip.
- Any curl transport failure (DNS, TLS, refused connection, cancelled fetch) is classified `Other` and surfaces as the calling operation's own failure path — a backfill/poll failure is swallowed per-conversation (silently skipped, not fatal to the whole pane), while a startup-critical call (`conversations.list`, `auth.test`, `users.list`) fails the whole `App::build`.
- An unknown/unparseable socket envelope is never a crash: it is acked-and-skipped (S5) with one log line, written to `$HERDR_PLUGIN_STATE_DIR/slackr.log` when that directory is set, a no-op otherwise.
- A socket gone silent without a read error (dead air — e.g. a firewall dropping packets rather than resetting the connection) is not treated as live forever: past a 90s liveness deadline (S8) since the last frame actually read, the loop errors out so `Down` fires and the polling fallback can engage.

## Related specs

- [overview](./overview.md)
- [config](./config.md)
- [pane](./pane.md)
