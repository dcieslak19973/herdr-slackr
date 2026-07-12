# herdr-slackr: a real-time Slack feed pane for herdr

**Date:** 2026-07-12
**Status:** Approved
**Repo:** `dcieslak19973/herdr-slackr`

## Goal

A read-only Slack pane inside herdr for a corporate environment: a live feed of selected
channels and DMs, and a mentions/triage view, so the reviewer never alt-tabs to Slack to
know whether something needs them. Real-time delivery via Slack Socket Mode, degrading to
polling when the socket can't run.

Out of scope by explicit choice: posting/replying (the user's existing Slack MCP covers
agent-side read/reply; humans reply in Slack), message persistence, and native herdr nav
integration (plugin v1 has no nav extension point — see §Nav presence).

## Constraints and context

- Corporate network: MITM proxy with a private CA; WebSockets may be blocked. The design
  must survive both.
- A corporate-approved Slack app is available. Two credentials:
  - **`xapp-…` app-level token** (`connections:open`) — opens the Socket Mode WebSocket.
  - **`xoxp-…` user OAuth token** — Web API for backfill and metadata. User-token scopes:
    `channels:read`, `channels:history`, `groups:read`, `groups:history`, `im:read`,
    `im:history`, `mpim:read`, `mpim:history`, `users:read`.
  - The app subscribes to **user events** `message.channels`, `message.groups`,
    `message.im`, `message.mpim` on behalf of the installing user, so events cover what
    the user can see, not what a bot was invited to.
- Distribution and build mirror `dcieslak19973/herdr-reviewr`: GitHub Releases, static
  musl Linux binaries + macOS, `install.sh` build step, clippy pedantic `-D warnings`,
  `unsafe_code = "forbid"`, specs in this voice.
- Platform: macOS + Linux (herdr's platforms). Development happens on Windows; tests must
  pass there.

## Architecture

Three units, mirroring reviewr's worker/UI split:

1. **Socket worker** (thread): owns the WebSocket. `apps.connections.open` (via the REST
   layer) yields a wss URL; connect → `hello` → deliver event envelopes to the app via a
   channel → ack each envelope by id → on `disconnect` frame or error, reconnect with
   exponential backoff (base 1s, cap 60s, jitter). This is the binary's only in-process
   networking: `tungstenite` (sync, no tokio) + `rustls` + `rustls-native-certs` (system
   trust store, so the corporate CA works) + `webpki-roots` fallback. All pure Rust —
   musl static builds survive, and CI's `ldd` staticness gate verifies it.
2. **REST layer**: every request/response call shells out to **curl** (the reviewr
   pattern — proxy/CA/redirect handling stays curl's problem): `apps.connections.open`,
   `conversations.list`, `conversations.history` (backfill, last 50 per channel on open),
   `conversations.replies` (thread expand), `users.list` (name cache, refreshed daily).
   Tokens ride a curl config on **stdin**, never argv.
3. **Pane** (ratatui): Feed and Mentions tabs over one in-memory message model.

**Polling fallback:** if the socket cannot connect, or drops and fails one full backoff
cycle, the app switches to polling `conversations.history` per subscribed channel every
`poll_fallback_secs` (default 30) via the REST layer, with a one-line status notice
("socket unavailable — polling"). Socket recovery switches back silently. The pane is
never dead because a proxy hates WebSockets.

## Tokens and config

- Token resolution order, per token: env (`SLACK_APP_TOKEN`, `SLACK_USER_TOKEN`), then
  `$HERDR_PLUGIN_CONFIG_DIR/tokens.toml` (`app_token = "xapp-…"`, `user_token = "xoxp-…"`);
  the file must not be group/world-readable (checked on Unix; refused with a remedy
  naming `chmod 600`). Tokens never appear in argv, logs, error strings, or the pane.
- `config.toml` in the same dir, reviewr's fail-loud contract (unknown key or invalid
  value blocks the pane with a path-naming error):

  ```toml
  channels = ["#eng-infra", "#releases"]   # required; names resolved to ids at startup
  dms = true                               # include IMs/MPIMs (default true)
  keywords = ["deploy", "oncall"]          # extra Mentions-tab triggers (default none)
  theme = "catppuccin-mocha"               # reviewr's palette system, same names
  poll_fallback_secs = 30                  # 5..=300
  ```

- A configured channel name that resolves to nothing is an error naming the channel; a
  channel the user cannot read surfaces Slack's error verbatim in the status line.

## The pane

- **Feed tab**: one chronological stream across subscribed conversations. Message rows:
  `#channel  @author  HH:MM` header + wrapped text. Thread replies collapse under their
  root with `↳ n replies`; Enter expands/collapses (replies fetched on first expand).
  An unread divider marks the first message that arrived since the user's last keypress
  in the pane (focus itself is invisible to a terminal child process, so input is the
  attention signal).
- **Mentions tab**: only rows that trigger attention — `@you` mentions, any DM/MPIM
  message, and keyword hits — newest first, each with a read marker toggled by `Enter`
  or cleared en masse. Unread mention count renders in the pane's tab bar.
- **Keys** (reviewr idiom, footer-hinted): `1`/`2` or `Tab` switch tabs, `j`/`k` move,
  `Enter` expand thread / toggle read, `o` open the selected message's permalink in the
  browser (`chat.getPermalink` via REST), `r` manual refresh/backfill, `q` quit pane.
- **Rendering**: message text is plain text — no mrkdwn styling interpretation — but
  Slack entities are resolved: `<@U…>` → `@name`, `<#C…|name>` → `#name`,
  `<url|label>` → label, `:emoji:` left as-is. Truncation and wrapping follow the pane
  width; no horizontal scroll.
- **Nav presence**: herdr's plugin v1 offers no nav extension point; the pane appears in
  the left nav's agent panel automatically (observed with reviewr on herdr 0.7.1). A
  **spike** during implementation tests whether an OSC 0/2 terminal-title escape updates
  that nav label (goal: `slack (3)` unread badge). If herdr doesn't reflect it, the count
  lives in the pane's own tab bar and the limitation is documented in the README.

## Manifest and plumbing

`herdr-plugin.toml` mirroring reviewr: pane entrypoint `feed` (placement `split`), actions
`toggle`/`open`/`close` via a `herdr/sidebar.sh` adapted from reviewr's (placement
configurable), `[[build]]` runs `herdr/install.sh` downloading the release binary for the
platform (musl triples on Linux). Plugin id `dcieslak19973.slackr`. No auto-open event —
the user opens it once per session (`toggle` bound to a key).

## Error handling

- Missing/invalid tokens → the pane renders a full-tab remedy naming the env vars and
  tokens.toml path (reviewr's degraded-state pattern). `invalid_auth`/`token_revoked`
  from Slack → same surface with Slack's error name.
- Rate limiting (HTTP 429) → honor `Retry-After`, status-line notice, no retry storm.
- Socket `disconnect` with `reason: link_disabled`/refresh requests → reconnect via a
  fresh `apps.connections.open` (Slack rotates socket URLs; never reuse one).
- Unknown/unparseable event envelopes → acked and skipped with a log line (never crash
  the worker); the log file lives in `HERDR_PLUGIN_STATE_DIR`.
- Clock skew, deleted/edited messages: `message_changed`/`message_deleted` subtypes
  update/remove the row in place; unknown subtypes render as plain messages.

## Testing

- Socket state machine (envelope parse → ack decision → reconnect policy) as pure
  functions against canned Socket Mode JSON (hello, events_api envelope, disconnect).
- Entity resolution and mention/keyword detection: table tests.
- Feed/Mentions model (ordering, unread divider, thread collapse, edit/delete subtypes):
  unit tests with fixture events.
- Render snapshots for both tabs (reviewr's tests/render.rs pattern).
- REST layer: curl-arg construction and response parsing with fixtures; the subprocess
  edge stays thin and untested (house pattern).
- The live WebSocket edge: manual smoke checklist in the README (run with real tokens,
  see a message arrive, kill the network, watch fallback engage).

## Non-goals

- No posting, reacting, or marking-as-read in Slack (read-only; Slack state is untouched).
- No message persistence across sessions.
- No Enterprise-Grid multi-workspace support (one workspace, one token pair).
- No native nav/badge integration beyond the title spike.
- No tokio; the socket worker is one thread with a sync WebSocket.
