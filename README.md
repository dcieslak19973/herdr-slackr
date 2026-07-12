# herdr-slackr

A real-time Slack feed pane for [herdr](https://herdr.dev). Your agent works in the chat pane;
Slack stays visible beside it, so you never alt-tab to check whether something needs you.

What you get, in one persistent pane:

- **Feed tab** — a live, chronological stream of the channels and DMs you subscribe to, one
  conversation history across all of them. Threads collapse under their root with `↳ n replies`;
  `Enter` expands one.
- **Mentions tab** — only what needs you: `@you` mentions, every DM/MPIM message, and your own
  keyword hits, newest first, with a per-row read marker.
- **Real-time delivery** via Slack Socket Mode, degrading to polling `conversations.history` when
  the socket can't run (a strict corporate proxy, a network blip) — the pane stays live either
  way, just slower.
- **Read-only.** It never posts, reacts, or marks anything read in Slack. Nothing it does is
  visible to anyone else in the workspace.

It is not a replacement for Slack — no composing, no reactions, no search. It is a always-visible
triage surface: glance at the pane, know if you need to context-switch, keep working if not.

## Requirements

- **herdr ≥ 0.7.0** (the plugin system).
- **macOS or Linux.**
- **curl** on `PATH` (the Web API backend — see [specs/slack-host.md](specs/slack-host.md)).
- A **truecolor (24-bit)** terminal. Pick a theme that matches its light or dark background (see
  [Theme](#theme)).
- A Slack app installed in your workspace with Socket Mode enabled and the scopes below —
  approved by your workspace admin if the workspace requires app-install review (see
  [Slack app setup](#slack-app-setup)).

## Slack app setup

herdr-slackr needs one Slack app with two credentials: an **app-level token** for Socket Mode and
a **user OAuth token** for the Web API backfill. This section is the exact checklist to hand to
whoever approves new Slack apps in your workspace — it lists every scope and subscription the app
requests, and nothing else.

1. Create a Slack app at <https://api.slack.com/apps> ("From scratch"), in your workspace.
2. **OAuth & Permissions → User Token Scopes** — add exactly these five:
   - `channels:read`, `channels:history`
   - `groups:read`, `groups:history`
   - `im:read`, `im:history`
   - `mpim:read`, `mpim:history`
   - `users:read`

   (Nine scopes total: four `:read`/`:history` pairs above, one per conversation kind, plus
   `users:read` for the display-name cache.)
3. **Socket Mode → Enable Socket Mode.** This generates the app-level token.
4. **Basic Information → App-Level Tokens → Generate Token and Scopes** — add the
   `connections:open` scope. Copy the token (`xapp-…`); this is `SLACK_APP_TOKEN` /
   `app_token` below.
5. **Event Subscriptions → Enable Events**, then under **Subscribe to events on behalf of
   users**, add:
   - `message.channels`
   - `message.groups`
   - `message.im`
   - `message.mpim`

   Subscribing on behalf of the *user* (not a bot) means events cover what you personally can
   see, not what a bot was invited into — you never have to `/invite` the app to a channel.
6. **Install App to Workspace** (or ask your admin to approve the install). Copy the **User OAuth
   Token** (`xoxp-…`); this is `SLACK_USER_TOKEN` / `user_token` below.

No bot token, no `chat:write`, no admin scopes. The app only ever reads what the installing user
can already see and opens one Socket Mode connection.

## Install

From the herdr marketplace. You get a prebuilt binary, no Rust toolchain:

```bash
herdr plugin install dcieslak19973/herdr-slackr
```

The feed does **not** auto-open — there is no `[[events]]` hook in the manifest. Bind a key to the
**slackr: toggle feed** action:

```toml
[[keys.command]]
key = "cmd+s"
type = "plugin_action"
command = "dcieslak19973.slackr.toggle"   # <plugin_id>.<action_id> — note the id, not the name
```

With no key bound, run the action once with
`herdr plugin action invoke toggle --plugin dcieslak19973.slackr`. `open` and `close` are the same
shape, made for scripts and layout plugins: `open` no-ops if the feed is already open, `close`
no-ops if none is.

## Tokens

herdr-slackr resolves each token independently, environment first:

| Token       | Env var             | `tokens.toml` key | Prefix   |
| ----------- | -------------------- | ----------------- | -------- |
| App-level   | `SLACK_APP_TOKEN`    | `app_token`        | `xapp-…` |
| User OAuth  | `SLACK_USER_TOKEN`   | `user_token`       | `xoxp-…` |

Set the env vars, or create:

```text
~/.config/herdr/plugins/config/dcieslak19973.slackr/tokens.toml
```

```toml
app_token = "xapp-your-token-here"
user_token = "xoxp-your-token-here"
```

herdr hands this directory to the plugin as `$HERDR_PLUGIN_CONFIG_DIR`; the path above is where it
lives on disk. On Unix, `tokens.toml` must not be readable by group or world — herdr-slackr refuses
to start otherwise, naming the exact `chmod 600 <path>` fix. A token with the wrong prefix (e.g. a
bot token where a user token belongs) is also refused; the error never echoes the token value
itself, in any form — logs, status line, or error text.

## Configuration

```text
~/.config/herdr/plugins/config/dcieslak19973.slackr/config.toml
```

```toml
channels = ["#eng-infra", "#releases"]   # required; names resolved to ids at startup
dms = true                               # include IMs/MPIMs (default true)
keywords = ["deploy", "oncall"]          # extra Mentions-tab triggers (default none)
theme = "catppuccin"                     # palette name (see Theme below)
poll_fallback_secs = 30                  # seconds between polls while the socket is down; 5..=300
```

`channels` is the only required key; every other key has a documented default. A missing config
file fails the same way a missing `channels` key does — `channels` has no default. An unknown key
or an invalid value for *any* key fails the **whole file** loudly: herdr-slackr never falls back to
partial defaults, because a typo that silently "just works" is worse than a pane that won't start
until you fix it. The pane shows the exact config path and what's wrong; fix the file and relaunch
the pane. See [specs/config.md](specs/config.md) for the full contract.

A configured channel name that doesn't resolve to a real channel is an error naming that channel.
A channel you can't read surfaces Slack's error verbatim in the status line.

### Theme

Reviewr's palette system, same names. `catppuccin` is the default:

- **Dark:** `catppuccin`, `catppuccin-frappe`, `catppuccin-macchiato`, `dracula`, `nord`,
  `gruvbox`, `one-dark`, `solarized`, `monokai`, `tokyo-night`, `rose-pine`.
- **Light:** `catppuccin-latte`, `gruvbox-light`, `one-light`, `solarized-light`, `github-light`,
  `tokyo-night-day`, `rose-pine-dawn`.

An unknown theme name does not block the pane — it's a warning, not a fail-loud config error: the
pane starts with the default palette and a one-line status notice naming the bad value. Pick one
that matches your terminal's light or dark background; the pane keeps the terminal's own
background.

## Controls

| Key           | Action                                                          |
| ------------- | ---------------------------------------------------------------- |
| `1` `2`       | Switch tab — Feed / Mentions                                     |
| `Tab`         | Switch tab (Feed ↔ Mentions)                                      |
| `j` `k` · `↑` `↓` | Move the cursor                                              |
| `Enter`       | Feed: expand/collapse the selected thread. Mentions: toggle read  |
| `o`           | Open the selected message's permalink in the browser             |
| `r`           | Manual refresh (re-pull the last 50 messages of every conversation) |
| `q`           | Quit the pane                                                     |

Any keypress moves the unread divider to "now" — the pane has no other way to detect that you've
looked at it (focus is invisible to a terminal child process), so input is the attention signal.

## Manual smoke checklist

herdr-slackr's socket/reconnect edge is not unit-testable (it's a real TLS WebSocket against
Slack's servers) — it's covered here instead, by hand, before each release:

1. **Run with real tokens.** Set `SLACK_APP_TOKEN`/`SLACK_USER_TOKEN` (or `tokens.toml`), open the
   pane inside herdr, confirm `channels`/`dms` from `config.toml` resolve without an error screen.
2. **See a live message.** Post in a subscribed channel or DM from another client. It should
   appear in the Feed tab within a second or two, and on the Mentions tab too if it's a DM or
   mentions you or hits a keyword.
3. **Kill the network.** A hard disconnect (turn off Wi-Fi) breaks the TCP connection outright,
   so the socket errors immediately and the status line should show `socket unavailable (…) —
   polling` within roughly 30 seconds (one backoff cycle). A firewall block instead (drop
   outbound to `slack.com` with no reply, rather than refusing it) produces dead air with no
   socket error, so the read loop has to wait out its 90-second liveness deadline (three 30s read
   timeouts) before it gives up and reconnects — expect the same status line within roughly 2
   minutes in that case.
4. **Verify polling still delivers.** Post another message while still offline-from-socket but
   with REST reachable (or restore just enough connectivity for `conversations.history` to
   answer) — it should appear within `poll_fallback_secs` of its send time.
5. **Restore the network.** The socket should reconnect on its own; the status line clears and
   `polling` stops appearing within one backoff interval.

## Limitations

This is a focused, young tool. Known constraints:

- **UTC timestamps.** Message times render as `HH:MM` in UTC (no timezone crate in the dependency
  list), not your local time.
- **No persistence.** Every message, read marker, and expanded thread lives in memory only — a
  pane restart re-backfills the last 50 messages per conversation from Slack and starts fresh.
- **Read-only, always.** No composing, replying, reacting, or marking read in Slack itself — this
  pane only ever reads.
- **No native nav badge, unverified.** The pane emits an OSC 0 terminal-title escape
  (`slack (n)`) naming the unread mention count, on the chance herdr's left-nav panel reflects a
  terminal-title update the way it does for reviewr's pane. Whether it actually does is a live-herdr
  question, not something a test can confirm — it needs to be checked against a running herdr
  once this plugin is installed, and this section updated with the result (tracked for Task 9).
  Until confirmed, treat the pane's own tab-bar count as the only reliable unread indicator.
- **One workspace, one token pair.** No multi-workspace / Enterprise Grid support.
- **No message search.** The Feed tab is a live stream, not a searchable archive.
- **macOS and Linux only** — no Windows pane (development happens on Windows; only the binary
  itself is cross-platform).

## Building from source

For contributors. `herdr plugin link` skips the download build step, so place a locally built
binary where the pane command looks for it, at `$HERDR_PLUGIN_ROOT/bin/herdr-slackr`:

```bash
git clone https://github.com/dcieslak19973/herdr-slackr
cd herdr-slackr
just install   # build release → bin/herdr-slackr
herdr plugin link .
```

The dev loop after the first link: edit the code, `just install`, then toggle the pane off and
back on with your keybind — the open pane keeps running the *old* process until you relaunch it.

`just ci` runs everything CI runs: `fmt-check`, `lint` (clippy pedantic, `-D warnings`), `test`,
and a release build.

## Design

The living design lives in [`specs/`](specs/), one concept per doc, always current:

- [specs/overview.md](specs/overview.md) — what this is, its invariants, and its scope.
- [specs/config.md](specs/config.md) — the config/token contracts.
- [specs/pane.md](specs/pane.md) — the Feed/Mentions UI: tabs, keys, markers, degraded states.
- [specs/slack-host.md](specs/slack-host.md) — Socket Mode lifecycle, the ack contract, URL
  rotation, polling fallback, and the REST methods + scopes this pane uses.

The original design proposal is [docs/superpowers/specs/2026-07-12-herdr-slackr-design.md](docs/superpowers/specs/2026-07-12-herdr-slackr-design.md).

## License

MIT.
