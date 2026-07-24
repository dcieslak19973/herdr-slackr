# herdr-slackr

A real-time Slack feed pane for [herdr](https://herdr.dev). Your agent works in the chat pane;
Slack stays visible beside it, so you never alt-tab to check whether something needs you.

What you get, in one persistent pane:

- **Feed tab** — a live, chronological stream of the channels and DMs you subscribe to, one
  conversation history across all of them, oldest at the top and newest at the bottom — like any
  chat client. Threads collapse under their root with `↳ n replies`; `Enter` expands one.
- **Mentions tab** — only what needs you: `@you` mentions, every DM/MPIM message, and your own
  keyword hits, oldest at the top and newest at the bottom, with a per-row read marker.
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
2. **OAuth & Permissions → User Token Scopes** — add exactly these nine:
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

> **Symptom of skipping step 5:** the pane connects, backfills history — and then never shows
> another message, because Socket Mode connects fine without any event subscriptions and a
> healthy-looking socket suppresses polling. The pane defends itself: after 5 minutes of total
> socket silence it runs a safety poll, and if that poll finds messages the socket should have
> delivered, the status line says `live events silent but polling found new messages — check the
> Slack app's event subscriptions` (with matching `safety poll:` lines in `slackr.log`). From
> that moment the pane also stops trusting the socket and polls at the normal
> `poll_fallback_secs` cadence (default 30s) until live events actually resume — so a
> misconfigured app degrades to a working, slightly-delayed feed rather than a frozen one. But
> the real fix is this checklist's step 5, on the app whose `xapp-` token you're using.

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

| Token       | Env var             | `tokens.toml` key | Prefix   | Required? |
| ----------- | -------------------- | ----------------- | -------- | --------- |
| App-level   | `SLACK_APP_TOKEN`    | `app_token`        | `xapp-…` | optional — omit for poll-only mode |
| User OAuth  | `SLACK_USER_TOKEN`   | `user_token`       | `xoxp-…` | required  |

**Poll-only mode.** Omitting the app token entirely (no env var, no `app_token` key) starts the
pane with no Socket Mode connection at all: the polling fallback becomes the permanent delivery
path (default every 30 seconds, request-budgeted, `r` still forces a full sweep), and the status
line carries a compact `poll-only` marker. Use this when you can't get a dedicated Slack app approved and the
only available `xapp-` token belongs to *another service's* app — **never share a Socket Mode app
between two consumers**: Slack load-balances each event to exactly one open connection, and this
pane acks what it receives, so a shared app means both consumers silently steal each other's
events. Live-message latency in poll-only mode is the polling interval, not instant; everything
else works identically. (A *malformed* `xapp-` token is still a loud error — only genuine absence
selects poll-only mode.)

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
dm_limit = 20                            # cap on subscribed DMs/MPIMs when dms=true; 0..=200
dm_allow = ["alice", "Bob Smith"]        # DM/MPIM names always subscribed, ignoring dm_limit (default none)
focus_keywords = ["incident", "p1"]      # Focus-view triggers, distinct from `keywords` (default none)
lookback_days = 7                        # how far back any history fetch reaches; 0..=365, 0 = unlimited
```

`channels` is the only required key; every other key has a documented default. A missing config
file fails the same way a missing `channels` key does — `channels` has no default. An unknown key
or an invalid value for *any* key fails the **whole file** loudly: herdr-slackr never falls back to
partial defaults, because a typo that silently "just works" is worse than a pane that won't start
until you fix it. The pane shows the exact config path and what's wrong; fix the file and relaunch
the pane. See [specs/config.md](specs/config.md) for the full contract.

A configured channel name that doesn't resolve to a real channel is an error naming that channel.
A channel you can't read surfaces Slack's error verbatim in the status line.

### Rate limits

herdr-slackr is deliberately conservative about how hard it hits Slack's Web API. The polling
fallback (while the socket is down) spends at most an 8-*request* budget per tick, round-robin
across every subscribed conversation, and asks Slack only for messages newer than the last one
already seen — a caught-up tick's response is typically empty rather than re-shipping the last 50
messages every time (and when a burst *is* larger than one page, the fetch follows Slack's cursor
for up to 10 pages so the middle of the burst isn't silently lost). The budget meters requests
rather than conversations because that's what Slack's rate limits meter: a caught-up conversation
costs one request, but a paginated gap fetch can cost up to ten, so right after a long outage — a
laptop asleep over lunch in a busy workspace — a batch automatically covers fewer conversations
per tick instead of multiplying its request volume tenfold at the exact moment Slack is most
likely to answer 429. If Slack answers with a real rate limit, the pane reads its actual
`Retry-After` value and pauses all polling until that deadline passes, rather than guessing at a
fixed backoff, and resumes the round-robin at the conversation it stopped at (whether the stop was
the rate limit or the budget) rather than skipping past it; a socket reconnect always clears a
pending cooldown immediately, since a healthy socket means Slack has already accepted the
connection. Because Socket Mode never redelivers events that fired while the connection was down,
every reconnect also arms a one-time catch-up sweep: each subscribed conversation gets one
watermarked history fetch, paced out in 8-request batches every 15 seconds — a conversation that
missed nothing answers with an empty body, so a sweep after a brief blip is close to free, while a
sweep over real gaps spreads itself out under the same budget.

`lookback_days` (default 7, valid `0..=365`, `0` = unlimited) is the *depth* companion to that
request-budget *rate* cap: startup backfill drops messages older than the horizon, and every
incremental fetch (polling, catch-up, the DM scan) clamps its "fetch since" bound to it. Without a
horizon, a watermark left over from a two-week gap would send pagination chasing history the
300-message retention cap would mostly discard anyway — pure request waste. With it, the deepest
any conversation's catch-up can reach is bounded no matter how long the pane was away.

**Shared app credentials.** Slack rate limits pool per app + workspace — not per pane, not per
person. If the Slack app behind your tokens is shared (several teammates running herdr-slackr, or
other tooling on the same bot), every consumer draws from the same budget. herdr-slackr's own
worst sustained rate is deliberately kept well under half of Tier 3 (~8 requests/30s polling,
~8/15s during a catch-up sweep, ≤2 per 5-minute DM scan) precisely so it behaves as a good tenant
on a shared key. Every recurring cadence — socket reconnects, poll-fallback ticks, catch-up
batches — carries ±25% jitter, because a Slack outage flips *every* pane on the key into polling
mode at the same moment, and without jitter their request batches would then fire in lockstep
against the shared pool indefinitely. Conversation listing always excludes archived channels,
which in an older workspace can otherwise double the page count of the startup list call for rows
a live feed could never use. If you still see `slack rate limit` notices while the pane is
idle-ish, the budget is being spent elsewhere on the same app, and raising `poll_fallback_secs`
and/or lowering `lookback_days` are the two knobs that cut this pane's share further. The workspace's `users.list`
directory (used for display names) is cached on disk for 24 hours in `$HERDR_PLUGIN_STATE_DIR`, so
a pane restart or a CLI invocation doesn't refetch the whole member list every time. Startup
backfill retries a rate-limited conversation exactly once (sleeping the real `Retry-After` first)
before giving up on the rest of the list for that session — the socket/poll paths fill in what
backfill couldn't.

> **Newer Slack apps have far tighter history limits.** Slack restricts non-Marketplace apps
> created after May 2025 to roughly one `conversations.history`/`conversations.replies` request
> per minute, with `limit` capped at 15. herdr-slackr's backfill and polling budgets assume the
> classic tiers (tens of requests per minute); on a new restricted app the pane will spend most of
> its polling-fallback life in `Retry-After` cooldowns and backfill will cover only a conversation
> or two. The live Socket Mode path is unaffected. If you can, register the Slack app before that
> cutoff's terms apply to you, or accept that polling mode will be slow to catch up.

`dm_limit` (default 20, valid `0..=200`) caps how many of your DMs/MPIMs are *actively subscribed*
— polled and backfilled on every regular tick — when `dms = true`, ranked by most-recently-active;
`0` means none are polled or backfilled by default. `dm_allow` (below) always-subscribes named DMs
regardless of this cap, on top of whichever ones rank inside it.

**Channels are strictly allow-list, in both delivery modes.** Only channels named in `channels`
are backfilled, polled, *or shown live*: Socket Mode delivers events for every conversation the
token can see — at a firm-sized workspace, that's every channel you've ever joined — and the pane
drops live events for any channel the config didn't name, rather than letting the whole workspace
firehose into the Feed. (Before this gate existed, an unconfigured channel's messages appeared
with a raw `#C…` id label; if you relied on that, name the channel in `channels`.)

**`dm_limit`, by contrast, never blocks a new message from arriving in any DM, capped or not, in
either delivery mode.** Over the live Socket Mode connection this is automatic: DM/MPIM events
pass the live gate regardless of subscription (including a DM first opened mid-session), so a
message on an out-of-cap DM shows up in the Feed/Mentions tabs immediately, same as any subscribed
one — unless `dms = false`, which suppresses live DM delivery the same way it suppresses DM
subscription. In polling mode there is a dedicated detection path for it: every 5 minutes, a scan re-fetches
the conversation list — DMs and MPIMs only, so the scan never pages through the workspace's public
channels — and checks every out-of-cap DM/MPIM's Slack-reported activity stamp
against what was last seen. If none moved, the scan costs nothing beyond that one list call. If one
or more did move, exactly one of them — the single most-recently-active, if several changed at once
— gets fetched with one extra `conversations.history` call that tick; the rest simply wait for the
next 5-minute scan to pick them up. This scan is skipped entirely (no list call, no history call)
during an active rate-limit cooldown from a prior 429, so it never adds pressure to a workspace
that Slack has already asked the pane to back off from.

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

Every row is color-segmented by field, not one flat color, under whichever palette is active: the
conversation label in the accent color, the author name in green, the time/thread markers in a
muted tone, and the message text in the default foreground.

## Sidebar badge

On herdr ≥ 0.7.4 the pane labels its own row in herdr's left sidebar with the unread mention
count — `slack (3)`, back to `slack` when you're read up — via `herdr pane report-metadata`.
No configuration needed; the title updates whenever the count changes.

The pane also publishes two custom metadata tokens for sidebar row layouts:

| token             | value                                                                 |
| ------------------ | ----------------------------------------------------------------------|
| `$slack_mentions` | the unread mention count (`0` when read up)                            |
| `$slack_link`     | `live` (socket delivering) · `polling` (poll-only mode or socket down) · `lossy` (socket connected but proven silent — see the safety poll) |

To render them, add the tokens to your herdr `config.toml` sidebar row layout (per-token
styling needs herdr ≥ 0.7.5), e.g. as an extra row under `[ui.sidebar.agents]`:

```toml
[ui.sidebar.agents]
rows = [
  ["state_icon", "workspace", "tab"],
  ["agent", { token = "$slack_mentions", bold = true }, { token = "$slack_link", dim = true }],
]
```

On herdr older than 0.7.4 the report fails once, writes one line to the plugin log, and the
pane never tries again that session — the badge quietly doesn't exist. The pane also still
emits an OSC 0 terminal-title escape (`slack (n)`) on every count change as a fallback for
those versions.

## Navigation

Every tab and every Feed projection reads top-to-bottom chronological, oldest at the top and
newest at the bottom — the same direction a chat client scrolls. **This is a change for the
Mentions tab and the Threads view**, which used to list newest-first; both now match the Feed
Timeline's direction instead of each having their own.

Real navigation keys move the cursor beyond one row at a time:

| Key                         | Action                                                        |
| ---------------------------- | ---------------------------------------------------------------|
| `G` / `End`                 | Jump to the newest row (the bottom)                            |
| `g` / `Home`                | Jump to the oldest row (the top)                                |
| `PageDown` / `PageUp`       | Move a full page (the pane's current on-screen row count)       |
| `Ctrl-d` / `Ctrl-u`         | Move a half page                                                |

**The `↓ n new` indicator.** When the cursor is scrolled up from the bottom of the active tab and
new messages arrive, a muted `↓ n new` overlay appears at the bottom-right of the row list,
counting every arrival since you last left the bottom. It clears the moment the cursor reaches the
bottom again, by any means — `j`/`↓`, `G`/`End`, a page move that lands there, or scrolling all the
way down manually. If the cursor is already sitting at the bottom when a message arrives, the view
follows it there automatically instead (like a chat client scrolled to "now") and the counter never
appears at all.

The counter is global, not scoped to what the active tab actually displays: it counts every new
message that lands anywhere in the message store (any subscribed conversation) while you're
scrolled up on the tab you're currently viewing, not only messages that would have added a visible
row to that tab. Scrolled up on the Mentions tab, for instance, a plain channel message that is
neither a DM nor a keyword/`@you` hit still bumps the counter, even though it never becomes a
Mentions row — it only ever shows up on the Feed tab. Treat it as "something arrived while you
were looking elsewhere," not as a precise per-tab row count.

## Threads view

`t` toggles the Feed tab (only) between two projections of the same message store:

- **Timeline** (default) — the chronological stream described above. A collapsed thread is
  exactly two rows: its root, and one enriched marker carrying the count *and* the latest reply —
  `↳ 3 replies · @alice: sounds good` — so a busy thread stays quiet in the feed while the marker
  itself tells you what just happened in it. (The count comes first deliberately: at any pane
  width, only the snippet ever gets clipped.) An expanded thread nests its replies beneath the
  root on a muted connector rail, padded so the columns line up:

  ```text
  #eng-infra  @carol  13:57  should we ship it?
  ├─          @dan    13:58  yes — after CI
  └─          @alice  13:59  agreed
  #eng-infra  @bob    14:02  unrelated message
  ```

  The leftmost character tells you the row type at a glance: `#`/`@` starts a message, `├`/`└`
  is inside a thread, `↳` is a collapsed thread's summary. New replies to a collapsed thread
  bump the marker's arrival, so the unread divider and the `↓ n new` counter still count them —
  page up to the divider and every new reply is there, at its thread.
- **Threads** — a triage digest of threads only, ordered by latest activity ascending (newest at
  the bottom — see [Navigation](#navigation)), so a thread that just got a reply jumps back to
  the bottom. Each thread is a **bold header row** — the root's text led by the reply count, its
  time column showing the thread's *latest activity* (the thing you're actually triaging by), not
  the root's age — followed by its newest three replies on the same connector rail; anything
  older collapses into one muted `… n earlier replies` line:

  ```text
  #eng-infra  @carol  13:59  2 replies · should we ship it?
  ├─          @dan    13:58  yes — after CI
  └─          @alice  13:59  agreed
  #release    @kim    14:02  5 replies · rollout plan?
  … 3 earlier replies
  ├─          @sam    13:40  staging green
  └─          @dan    14:02  starting now
  ```

  Non-threaded messages are excluded entirely. Threads whose root predates the pane's history
  window get a synthetic `n replies · (thread — root not loaded)` header that self-heals once
  the root is fetched.
- **`Enter` in the Threads view** always (re)fetches the selected thread's replies over REST —
  from the header, a reply row, or the `… n earlier replies` line alike (there is no "collapsed"
  state here for a toggle to mean, and refetching from the overflow line is how you pull the full
  thread in).
- **Discoverable expansion, anywhere on a thread.** In the Timeline, `Enter` expands/collapses a
  thread from its marker row, the thread's own root message row, or any rail row once expanded —
  you never have to hunt for the exact marker row. Expanding or collapsing sets a one-line status
  confirming what happened: `thread expanded — n replies` (`thread expanded — 1 reply` for
  exactly one) or `thread collapsed`. The footer also shows an `enter expand/collapse thread`
  hint whenever the selected row would actually do something thread-related.
- **Orphaned threads self-heal.** A reply whose root was never backfilled or seen still shows up
  here, as a synthetic entry headed `(thread — root not loaded)` instead of being dropped.
  Selecting it and hitting `Enter` fetches the real root over REST like any other refresh; once
  it arrives, the next redraw quietly replaces the placeholder with the real root row — no
  separate action needed.
- **Polling reply-refresh.** While the pane is in fallback polling mode, up to 2 of each tick's
  8-request budget rotate round-robin over "active" threads (currently expanded in the Timeline,
  or whose Slack-reported `reply_count` outpaces what's stored locally) to fetch just the newer
  replies, within the same total per-tick budget described in [Rate limits](#rate-limits) — not
  in addition to it. With no active threads, the full budget goes to conversations as before.

## Focus mode

`f` toggles the Feed tab (only) into and out of a third projection, **Focus** — a narrower view
than either the Timeline or Threads: only messages that (a) arrived live during this run of the
pane, and (b) either came from an allow-listed DM/MPIM (`dm_allow`) or hit a `focus_keywords`
trigger. Anything backfilled at startup is excluded, even if it would otherwise qualify — Focus is
"what needs my attention right now", not a filtered history search. Restarting the pane resets what
counts as "since app start"; there is no persistence across sessions.

A message qualifies for Focus the same way regardless of which condition it hits:

- **Allow-list match** — the message's conversation is one of the DM/MPIM names in `dm_allow`
  (exact, case-insensitive — no substring matching, same rule `resolve_channels` uses).
- **Keyword match** — the message text contains a `focus_keywords` entry, case-insensitively, as a
  substring (the same matching rule `keywords` uses for Mentions, but a distinct list — setting one
  never affects the other).

Either condition alone is enough (an OR, not an AND); the message just needs to have arrived live.

`t` (Threads) and `f` (Focus) are mutually exclusive Feed-tab views, each toggled by its own key
rather than one shared three-way cycle — pressing one while the other is active switches straight
to it instead of first returning to the Timeline:

| before → key | `t`        | `f`        |
| ------------- | ---------- | ---------- |
| `Timeline`    | `Threads`  | `Focus`    |
| `Threads`     | `Timeline` | `Focus`    |
| `Focus`       | `Threads`  | `Timeline` |

For example: from the Timeline, `t` lands on Threads; pressing `f` from there jumps straight to
Focus (not back through Timeline first); pressing `t` again from Focus lands back on Threads, not
Timeline.

## Controls

| Key           | Action                                                          |
| ------------- | ---------------------------------------------------------------- |
| `1` `2`       | Switch tab — Feed / Mentions                                     |
| `Tab`         | Switch tab (Feed ↔ Mentions)                                      |
| `j` `k` · `↑` `↓` | Move the cursor                                              |
| `G` · `End`   | Jump to the newest row (the bottom) — see [Navigation](#navigation) |
| `g` · `Home`  | Jump to the oldest row (the top)                                  |
| `PageDown` · `PageUp` | Move a full page                                          |
| `Ctrl-d` · `Ctrl-u` | Move a half page                                            |
| `Enter`       | Feed timeline: expand/collapse the selected thread (root, marker, reply, or activity row — see [Threads view](#threads-view)). Feed threads view: (re)fetch the selected thread's replies. Mentions: toggle read |
| `t`           | Feed tab only: toggle the Feed between the Timeline and the Threads-only view |
| `f`           | Feed tab only: toggle the Feed into/out of the Focus view (see [Focus mode](#focus-mode)) |
| `o`           | Open the selected message's permalink in the browser             |
| `r`           | Manual refresh: polls one batch immediately, then sweeps every subscribed conversation from its watermark in paced, rate-budgeted batches (status counts down `refreshing n conversations` and ends `HH:MM refresh complete`) |
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
   socket error, so the read loop has to wait out its 90-second liveness deadline — the deadline
   trips on the first timeout tick past 90s, in practice the fourth 30s read timeout — before it
   gives up and reconnects — expect the same status line within roughly 2 minutes in that case.
4. **Verify polling still delivers.** Post another message while still offline-from-socket but
   with REST reachable (or restore just enough connectivity for `conversations.history` to
   answer) — it should appear within `poll_fallback_secs` of its send time.
5. **Restore the network.** The socket should reconnect on its own; the status line clears and
   `polling` stops appearing within one backoff interval.
6. **Sidebar badge** (herdr ≥ 0.7.5). Receive a DM: the pane's sidebar row should read
   `slack (1)` within a tick; mark it read (`Enter` on the Mentions row) and the row returns
   to `slack`. On herdr < 0.7.4: at most one `sidebar badge: …` line in the plugin log
   (`herdr plugin log list --plugin dcieslak19973.slackr` — written on the next feed activity
   after the failure), and no other visible behavior.

## Working with agents

The pane is for you to watch. Your coding agent gets its own read-only view of the same Slack
feed through subcommands on this same binary — `mentions` and `feed` — so it can check what
needs you without you alt-tabbing to Slack. See
[specs/agent-cli.md](specs/agent-cli.md) for the full CLI contract.

### Install the skill

The universal path works across harnesses — Claude Code, Gemini CLI, GitHub Copilot, OpenCode,
Amp, Codex and more — via the [skills CLI](https://github.com/skills-sh/skills), verified working
against this repo:

```bash
npx skills add dcieslak19973/herdr-slackr --skill herdr-slackr -g
```

`-g` installs globally (every harness's personal skills directory, e.g. `~/.claude/skills` for
Claude Code); omit it to install per-project instead, into each harness's project-level directory
in the current repo. Either way, once installed it's in every session's skill list: "check my
Slack mentions" works with no `skill-path`/`load that skill` preamble.

If you'd rather not use `npx`, `herdr-slackr` installs the skill itself, offline, from the
already-installed plugin — no npm required. After `herdr plugin install`, the binary is available
as `herdr-slackr` *if* `~/.local/bin` is on your `PATH` (`install.sh` links it there; see
[Install](#install)):

```bash
herdr-slackr skill-install             # ~/.claude/skills/herdr-slackr (Claude Code, personal)
herdr-slackr skill-install --project   # ./.agents/skills/herdr-slackr (universal, project-level)
```

If `~/.local/bin` isn't on `PATH`, skip the bare command and invoke the plugin action instead,
which runs the same binary by its plugin-root path and needs no `PATH` entry:

```bash
herdr plugin action invoke skill-install --plugin dcieslak19973.slackr
```

`--project` installs into `.agents/skills/`, the location read by Gemini CLI, GitHub Copilot,
OpenCode, Amp, Antigravity and others (Claude Code reads it too, via the skills ecosystem
tooling; Codex and Cursor also read `.claude/skills/`). Commit that path and every agent session
opened in the repo picks it up, no per-user install step at all. `--project` and `--target` are
mutually exclusive.

Variants, either mode: `--copy` installs a real file instead of a symlink (Windows falls back to
this automatically, with a note on stderr); `--target <dir>` installs somewhere else entirely.
Re-running is idempotent: an unchanged install prints `already installed at <path>` and exits 0. A
conflicting file at the target exits 1 naming it; add `--force` to replace it.

### Make it proactive (CLAUDE.md)

Installing the skill covers "the agent knows how, once asked." It doesn't make the agent check
Slack unprompted — for that, put this in your `CLAUDE.md` (loaded every session, unlike the skill
list, which is only consulted when the agent decides it's relevant):

```
Slack triage happens via herdr-slackr — when the user asks about mentions or a
channel, run `herdr-slackr mentions --json` / `herdr-slackr feed --channel …`.
```

`skill-install` prints this same snippet after a fresh install, as a copy-pasteable reminder.
Without it, the most common failure mode is the agent simply not knowing slackr exists until you
say so.

### Reading the feed

```bash
herdr-slackr mentions --json          # @you, every DM/MPIM, and keyword hits, newest first
herdr-slackr feed --channel "#eng-infra" --json   # recent history in one configured conversation
```

Both take `--limit <n>` (default 20). Human output (the default, no `--json`) renders
`#chan  @author  HH:MM  text`; `--json` emits the raw message documents plus resolved
conversation names, for an agent to parse. Every invocation is a fresh, independent Slack REST
read — the CLI does not talk to a running pane — using the same config and token discovery as the
pane (`$HERDR_PLUGIN_CONFIG_DIR`, else `~/.config/herdr/plugins/config/dcieslak19973.slackr/`).

**READ-ONLY, always.** Neither subcommand ever posts, reacts, or marks anything read — same
invariant as the pane. An agent that needs to reply does so through your Slack MCP integration
(if configured) or by telling you to send it yourself; this CLI has no write path at all.

## Limitations

This is a focused, young tool. Known constraints:

- **UTC timestamps.** Message times render as `HH:MM` in UTC (no timezone crate in the dependency
  list), not your local time. A message from a UTC calendar day earlier than today additionally
  shows the date, as `Mon DD HH:MM` (e.g. `Jul 12 06:00`), so a prior day's message is never
  mistaken for one from today.
- **No persistence.** Every message, read marker, and expanded thread lives in memory only — a
  pane restart re-backfills the last 50 messages per conversation from Slack and starts fresh.
- **Read-only, always.** No composing, replying, reacting, or marking read in Slack itself — this
  pane only ever reads.
- **Sidebar badge needs herdr ≥ 0.7.4.** On older herdr the `pane report-metadata` call fails
  once, logs once, and stays off for the session (see [Sidebar badge](#sidebar-badge)); the
  OSC 0 terminal-title escape (`slack (n)`) remains the only — unverified — nav signal there,
  and the pane's own tab-bar count the only reliable one.
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
- [specs/agent-cli.md](specs/agent-cli.md) — the `mentions`/`feed`/`skill-path`/`skill-install`
  CLI contract, config/token discovery outside herdr, and partial-results semantics.

The original design proposals are
[docs/superpowers/specs/2026-07-12-herdr-slackr-design.md](docs/superpowers/specs/2026-07-12-herdr-slackr-design.md)
and [docs/superpowers/specs/2026-07-12-agent-cli-design.md](docs/superpowers/specs/2026-07-12-agent-cli-design.md).

## License

MIT.
