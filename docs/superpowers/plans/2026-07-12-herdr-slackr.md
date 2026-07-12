# herdr-slackr Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A read-only real-time Slack feed pane for herdr — Socket Mode WebSocket with automatic polling fallback, Feed + Mentions tabs.

**Architecture:** Three units: a socket worker thread (`tungstenite` + `rustls`, the binary's only in-process networking), a REST layer that shells out to `curl` for every request/response call (tokens via stdin curl config), and a ratatui pane over one in-memory message model. Everything not the live socket mirrors `D:\git\herdr-reviewr` — the local pattern source implementers read directly.

**Tech Stack:** Rust 2024. Dependencies: `anyhow`, `ratatui 0.30`, `serde_json`, `toml 0.8`, `unicode-width`, plus `tungstenite` (sync, **no tokio**), `rustls`, `rustls-native-certs`, `webpki-roots`. Dev: `tempfile`. Nothing else.

## Global Constraints

- **Spec:** `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md` — the contract; read it first.
- Dependency list above is CLOSED — adding any other crate is a plan violation. No tokio, ever.
- Tokens (`xapp-…`, `xoxp-…`) never in argv, logs, error strings, or rendered output. REST tokens ride a curl config on stdin (`--config -`), the reviewr-proven pattern.
- Lints: copy reviewr's `Cargo.toml` `[lints]` tables verbatim (`unsafe_code = "forbid"`, clippy pedantic warn + the same allows). CI treats warnings as errors.
- Pattern source: `D:\git\herdr-reviewr` — when a task says "adapt reviewr's X", read that file and keep its structure, comment voice, and error-handling shape; change only what the task names.
- Verification gate before every commit: `cargo fmt --all` (discard CRLF-only rewrites of untouched files — check `git diff --ignore-cr-at-eol --stat`; content diffs are real), `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`.
- Branch: work directly on `main` until Task 9's PR note (greenfield repo, no consumers). Commits end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Windows dev box: everything must compile and test on Windows even though the plugin targets macos/linux (guard Unix-only bits like the 0600 check with `#[cfg(unix)]`, as reviewr does).

---

### Task 1: Scaffold — crate, lints, CI, justfile

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `justfile`, `.gitignore`, `.github/workflows/ci.yml`, `src/main.rs`, `src/lib.rs`

**Interfaces:**
- Produces: a compiling crate named `herdr-slackr` (binary `herdr-slackr`) whose `main` prints `herdr-slackr <version>` for `--version` and otherwise exits with `"herdr-slackr: the pane must run inside herdr (set HERDR_PLUGIN_CONFIG_DIR)"` on stderr, exit 1 — the TUI arrives in Task 7. `src/lib.rs` starts empty except module declarations added by later tasks.

- [ ] **Step 1:** Copy `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `.gitignore` from `D:\git\herdr-reviewr` verbatim (add `.superpowers/` to .gitignore if not present). Write `Cargo.toml` with the closed dependency list above, `edition = "2024"`, `rust-version = "1.90"`, `repository = "https://github.com/dcieslak19973/herdr-slackr"`, and reviewr's `[lints]` + `[profile.release]` tables verbatim.
- [ ] **Step 2:** Adapt reviewr's `justfile` (same tasks; binary name changes) and `.github/workflows/ci.yml` — same two jobs: `fmt · clippy · test` and the `static-linux` musl `ldd` gate (now load-bearing: rustls must not drag in dynamic OpenSSL; the gate proves it).
- [ ] **Step 3:** `src/main.rs` per the Produces block. Write the failing test first in `tests/smoke.rs`: spawn the binary (`env!("CARGO_BIN_EXE_herdr-slackr")`) with `--version`, assert stdout starts `herdr-slackr 0.1`; spawn bare with empty env addition, assert exit 1 and the stderr line.
- [ ] **Step 4:** Run `cargo test` — RED (no binary), implement, GREEN. Full gate.
- [ ] **Step 5:** Commit `chore: scaffold crate, lints, CI`.

---

### Task 2: Config and tokens (`src/config.rs`, `src/tokens.rs`)

**Files:**
- Create: `src/config.rs`, `src/tokens.rs`

**Interfaces (produced):**
```rust
// config.rs — adapt reviewr's src/config.rs machinery (fail-loud PluginConfig, KNOWN keys,
// value_error shape, plugin_config_in(dir) test seam). Keys per spec §Tokens and config:
pub struct PluginConfig { /* private fields */ }
impl PluginConfig {
    pub fn channels(&self) -> &[String];          // required, nonempty, each starts with '#'
    pub fn dms(&self) -> bool;                    // default true
    pub fn keywords(&self) -> &[String];          // default empty, matched case-insensitively
    pub fn theme(&self) -> &str;                  // default "catppuccin-mocha"
    pub fn poll_fallback_secs(&self) -> u64;      // default 30, valid 5..=300
}
pub fn plugin_config() -> Result<PluginConfig, PluginConfigError>;          // $HERDR_PLUGIN_CONFIG_DIR/config.toml
pub fn plugin_config_in(dir: impl AsRef<Path>) -> Result<PluginConfig, PluginConfigError>;

// tokens.rs
pub struct Tokens { pub app: String, pub user: String }   // xapp-…, xoxp-…
#[derive(Debug)] pub struct TokenError(pub String);       // remedy text, never a token value
/// Resolution per token: env (SLACK_APP_TOKEN / SLACK_USER_TOKEN, non-empty) → tokens.toml
/// in `dir` (keys app_token/user_token). On Unix, a tokens.toml readable by group/world is
/// refused with a chmod 600 remedy. Prefix-validated: app starts "xapp-", user "xoxp-";
/// wrong prefix is an error naming the expected prefix, never echoing the value.
pub fn resolve(dir: &Path, env: impl Fn(&str) -> Option<String>) -> Result<Tokens, TokenError>;
```

- [ ] **Step 1: Failing tests.** Config: mirror reviewr's config test suite shape — defaults, each key parses, invalid values fail loud naming the key, unknown key fails, missing `channels` fails, `channels = []` fails, `poll_fallback_secs = 3` and `301` fail. Tokens (in `src/tokens.rs` tests, injected env closure — no real env mutation): env wins over file; file used when env absent; missing both → error naming both sources; `xoxb-` in the user slot → error naming `xoxp-` and NOT containing the passed value (assert `!msg.contains("xoxb")` on a fake token string); `#[cfg(unix)]` perm test with a 0644 tokens.toml → refused with "chmod 600" in the message.
- [ ] **Step 2:** RED. **Step 3:** implement (config.rs adapted from reviewr; tokens.rs fresh, ~120 lines). **Step 4:** full gate GREEN. **Step 5:** commit `feat: config and token resolution`.

---

### Task 3: Slack model and text entities (`src/model.rs`, `src/entities.rs`)

**Files:**
- Create: `src/model.rs`, `src/entities.rs`

**Interfaces (produced):**
```rust
// model.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConvKind { Channel, Group, Im, Mpim }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation { pub id: String, pub name: String, pub kind: ConvKind }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub conv: String,            // conversation id
    pub ts: String,              // Slack ts ("1752300000.000100") — identity + sort key
    pub thread_ts: Option<String>, // Some(root ts) when this is a thread reply
    pub author: String,          // user id, resolved to a name at render time
    pub text: String,            // raw Slack text; entities resolved at render
    pub edited: bool,
}
/// Slack ts strings compare correctly as (f64 seconds, seq) — compare numerically via
/// split at '.', NOT lexically (unpadded seconds break lexical order across 10^n).
pub fn ts_cmp(a: &str, b: &str) -> std::cmp::Ordering;

// entities.rs
/// Resolve Slack entities for display: <@U123> → @name (lookup), <#C123|eng> → #eng,
/// <#C123> → #id-fallback, <https://x|label> → label, <https://x> → https://x,
/// &lt;/&gt;/&amp; unescaped. Unknown users render @U123. Pure; lookup injected.
pub fn resolve(text: &str, user_name: impl Fn(&str) -> Option<String>,
               conv_name: impl Fn(&str) -> Option<String>) -> String;
/// Attention detection for the Mentions tab: literal <@{self_id}> mention, any Im/Mpim
/// message, or a case-insensitive keyword hit in the RAW text.
pub fn is_mention(msg: &Message, kind: ConvKind, self_id: &str, keywords: &[String]) -> bool;
```

- [ ] **Step 1: Failing table tests.** `ts_cmp`: `"999.1" < "1000.0"` (the lexical trap), equal, seq tiebreak `"1.000001" < "1.000002"`. `resolve`: each entity form above plus a mixed sentence, unknown user, HTML entities. `is_mention`: mention hit, DM always, keyword case-insensitive, keyword-in-word behavior (pick substring match; document it), no-hit channel message.
- [ ] **Step 2:** RED. **Step 3:** implement (both files pure, no I/O). **Step 4:** gate GREEN. **Step 5:** commit `feat: slack message model and entity resolution`.

---

### Task 4: REST layer via curl (`src/proc.rs`, `src/rest.rs`)

**Files:**
- Create: `src/proc.rs` (adapt reviewr's `src/forge/proc.rs` run_tool verbatim minus the env variant), `src/rest.rs`

**Interfaces (produced):**
```rust
// rest.rs — every call: curl --silent --show-error --fail --config - <url>, stdin config
// carries `header = "Authorization: Bearer <token>"`. Slack wraps errors as
// {"ok":false,"error":"..."} with HTTP 200 — check `ok` on every response.
pub struct Rest<'a> { pub user_token: &'a str, pub cancelled: &'a AtomicBool }
#[derive(Debug, PartialEq, Eq)]
pub enum RestError {
    NoCurl,
    SlackError(String),          // the `error` field: invalid_auth, channel_not_found, …
    RateLimited(u64),            // Retry-After seconds
    Other(String),
}
impl Rest<'_> {
    /// GET method with query params (percent-encoded values; reuse an `enc` fn copied from
    /// reviewr's forge/mod.rs), parse JSON, map !ok / 429 / transport to RestError.
    pub fn get(&self, method: &str, params: &[(&str, &str)]) -> Result<serde_json::Value, RestError>;
}
/// The socket worker's one REST need, with the APP token instead: POST apps.connections.open,
/// returns the wss:// URL.
pub fn connections_open(app_token: &str, cancelled: &AtomicBool) -> Result<String, RestError>;

// Typed wrappers (each = one get() + one pure parse fn, parse fns unit-tested on fixtures):
pub fn list_conversations(rest: &Rest) -> Result<Vec<crate::model::Conversation>, RestError>;   // conversations.list, types=public_channel,private_channel,im,mpim, paginate cursor
pub fn history(rest: &Rest, conv: &str, limit: u32) -> Result<Vec<crate::model::Message>, RestError>;
pub fn replies(rest: &Rest, conv: &str, thread_ts: &str) -> Result<Vec<crate::model::Message>, RestError>;
pub fn users(rest: &Rest) -> Result<Vec<(String, String)>, RestError>;                          // (id, display or real name), paginate
pub fn permalink(rest: &Rest, conv: &str, ts: &str) -> Result<String, RestError>;               // chat.getPermalink
pub fn auth_self(rest: &Rest) -> Result<String, RestError>;                                     // auth.test → user_id (the self id for mention detection)
```
Base URL `https://slack.com/api/`. 429 handling: parse `Retry-After` from curl's `--write-out`? No — keep curl simple: on `--fail` + 429 curl exits 22 with the status in stderr; instead run WITHOUT `--fail`, always parse the body, and read rate-limit info from Slack's JSON error `ratelimited` (Slack includes it) with a default 30s. Document this in the module doc.

- [ ] **Step 1: Failing tests.** Parse fns against realistic fixture JSON: conversations.list (mix of kinds, name_normalized for channels, `user` id for IMs — an IM's display name is the counterpart user id, resolved later), history (messages array with ts/user/text/thread_ts/edited, ordering), users.list (profile.display_name falling back to real_name), auth.test, error body `{"ok":false,"error":"invalid_auth"}` → SlackError("invalid_auth"), `ratelimited` → RateLimited(30). Curl-arg construction: token in stdin config not argv (assert the args vector), enc() of params.
- [ ] **Step 2:** RED. **Step 3:** implement. **Step 4:** gate GREEN. **Step 5:** commit `feat: curl REST layer with typed slack calls`.

---

### Task 5: Socket worker (`src/socket.rs`)

**Files:**
- Create: `src/socket.rs`

**Interfaces (produced):**
```rust
/// What the worker sends the app over an mpsc channel.
#[derive(Debug, PartialEq, Eq)]
pub enum SocketEvent {
    Connected,
    /// A message-family event already mapped to the model (message / message_changed /
    /// message_deleted), with the conversation id and kind hint from the envelope.
    Message(crate::model::Message),
    Changed(crate::model::Message),
    Deleted { conv: String, ts: String },
    /// The socket is down and the worker is backing off; the app should poll.
    Down(String),
}
/// PURE state machine core (unit-tested): given one raw websocket text frame, produce
/// (events to emit, Option<ack envelope_id to send back>). Handles: hello → [Connected];
/// events_api envelope wrapping a message subtype → mapped event + ack; disconnect frame
/// → [Down(reason)] + no ack; unknown envelope type or unparseable payload → [] + ack
/// (ack anyway so Slack doesn't redeliver garbage). Subtypes: none/bot_message → Message,
/// message_changed → Changed (inner message), message_deleted → Deleted, others → ignore.
pub fn handle_frame(frame: &str) -> (Vec<SocketEvent>, Option<String>);
/// Reconnect schedule: attempt n (0-based) → seconds. 1,2,4,…,60 cap, ±25% jitter
/// (jitter injected as a fn for testability).
pub fn backoff_secs(attempt: u32, jitter: impl Fn(u64) -> u64) -> u64;
/// The worker loop (thin, integration edge): connections_open → tungstenite connect
/// (rustls ClientConfig with rustls_native_certs roots + webpki_roots fallback) → read
/// frames → handle_frame → send events / write acks ({"envelope_id":"…"}) → on error or
/// Down, emit Down, sleep backoff, repeat until `cancelled`. Runs on its own thread.
pub fn run(app_token: String, tx: std::sync::mpsc::Sender<SocketEvent>, cancelled: Arc<AtomicBool>);
```

- [ ] **Step 1: Failing tests for the pure core.** Fixtures: a real-shaped `hello` frame; an `events_api` envelope (`{"envelope_id":"x","type":"events_api","payload":{"event":{"type":"message","channel":"C1","ts":"1.2","user":"U1","text":"hi"}}}`) → `[Message(..)]` + ack "x"; `message_changed` (nested `message` object) → Changed; `message_deleted` (`deleted_ts`) → Deleted; `disconnect` frame (`{"type":"disconnect","reason":"refresh_requested"}`) → Down + no ack; garbage `"{"` → `[]` + no ack (unparseable frame has no envelope id); unknown envelope with an id → `[]` + ack. `backoff_secs`: 0→1, 1→2, 5→32, 8→60 cap (identity jitter), jitter fn actually applied.
- [ ] **Step 2:** RED. **Step 3:** implement `handle_frame`/`backoff_secs` pure, then the `run` loop (~80 lines, untested integration edge with a dense doc comment explaining the ack and URL-rotation contract: every reconnect calls connections_open afresh, never reuses a wss URL — spec §Error handling).
- [ ] **Step 4:** gate GREEN. **Step 5:** commit `feat: socket-mode worker with tested frame handling`.

---

### Task 6: Feed state (`src/app.rs`)

**Files:**
- Create: `src/app.rs`

**Interfaces (produced — Task 7 renders exactly these):**
```rust
pub enum Tab { Feed, Mentions }
pub struct App {
    pub tab: Tab,
    pub cursor: usize,                     // index into visible_rows() of the active tab
    pub status: String,                    // one-line notice (socket down, rate limit, …)
    pub polling: bool,                     // true while in fallback mode (renders in status)
    /* private: conversations, name caches, messages (BTreeMap<(conv, ts-key)>), mentions
       read-set, unread divider ts, expanded threads, self_id, keywords */
}
impl App {
    pub fn build(config: PluginConfig, tokens: &Tokens, rest: &Rest) -> Result<App, String>;
    // ^ resolves channels→ids via list_conversations (error names unknown channels),
    //   auth_self, users cache, then history backfill (limit 50) per subscribed conv.
    pub fn apply(&mut self, ev: SocketEvent);                  // insert/replace/delete + mention detect
    pub fn poll_tick(&mut self, rest: &Rest);                  // fallback: history since last ts per conv
    pub fn feed_rows(&self) -> Vec<Row>;                       // chronological, thread-collapsed
    pub fn mention_rows(&self) -> Vec<Row>;                    // newest first, read markers
    pub fn unread_mentions(&self) -> usize;
    pub fn touch(&mut self);                                   // any keypress: moves the unread divider
    pub fn toggle_expand_or_read(&mut self, rest: &Rest);      // Enter semantics per tab
    pub fn permalink_of_selected(&self, rest: &Rest) -> Option<String>;
}
pub struct Row { pub conv_label: String, pub author: String, pub time_hhmm: String,
                 pub text: String, pub kind: RowKind }
pub enum RowKind { Message, ThreadMarker { replies: usize, expanded: bool },
                   Divider, Mention { read: bool } }
```
Ordering: BTreeMap keyed by a sortable ts key derived via `model::ts_cmp` semantics (e.g. `(u64 secs, u32 seq)` tuple). Edits replace text + set `edited`; deletes remove; a reply increments its root's ThreadMarker count and only renders inline when expanded (fetch via `replies()` on first expand). The unread divider sits before the first message with arrival ts later than the last `touch()`.

- [ ] **Step 1: Failing tests** (unit, in-module, feeding synthetic `SocketEvent`s — no I/O; `build` gets a thin injectable seam or is exercised in Task 9's live smoke only, your call, but apply/rows/mention logic must be pure-tested): ordering across convs, edit updates in place, delete removes, thread collapse + count, divider placement after touch, mention read toggle, unread_mentions count, poll dedup (a message arriving via both poll and socket appears once — keyed by (conv, ts)).
- [ ] **Step 2:** RED. **Step 3:** implement. **Step 4:** gate GREEN. **Step 5:** commit `feat: feed and mentions state model`.

---

### Task 7: Pane UI (`src/ui.rs`, `src/theme.rs`, event loop in `src/lib.rs`, real `main`)

**Files:**
- Create: `src/ui.rs`, `src/theme.rs` (copy reviewr's `src/theme.rs` palette system verbatim — same theme names so the spec's `theme` key works), `src/lib.rs` `run()`
- Modify: `src/main.rs` (launch `run()`)
- Test: `tests/render.rs`

**Interfaces:**
- Consumes: `App` rows/API from Task 6, `SocketEvent` channel from Task 5, theme from config.
- Produces: the working pane. Event loop: crossterm events with a 250ms tick; drain the socket mpsc each tick (`apply`); when the latest `SocketEvent::Down` is unresolved for a full backoff cycle, flip `polling` and call `poll_tick` every `poll_fallback_secs` until a `Connected` arrives (spec §Polling fallback). Keys: `1`/`2`/`Tab` tabs, `j`/`k` move, `Enter` = `toggle_expand_or_read`, `o` open permalink (spawn `open`/`xdg-open`, the reviewr browser pattern if one exists — check reviewr's `o` handling in ui/lib and copy), `r` re-backfill, `q` quit. Every keypress calls `app.touch()` first. Tab bar shows `1 Feed  2 Mentions (n)` with n = unread_mentions; status line renders `app.status` + `polling` marker. Missing/invalid tokens or config: full-pane remedy screen (reviewr's degraded-state pattern), no TUI crash.
- OSC title spike (spec §Nav presence): on unread_mentions change, emit `\x1b]0;slack ({n})\x07` to stdout before the ratatui draw; document in the report whether herdr's nav label reflects it (testable only in live smoke; the emission itself gets a unit test on the escape-string builder).

- [ ] **Step 1: Failing render tests** (adapt reviewr's tests/render.rs harness: render App into a test backend buffer): feed rows show `#chan @author HH:MM text`; thread marker `↳ 2 replies`; divider line renders; mentions tab shows read/unread markers and the tab-bar count; degraded token screen names SLACK_APP_TOKEN/SLACK_USER_TOKEN and the tokens.toml path; escape-builder test `nav_title(3) == "\x1b]0;slack (3)\x07"`.
- [ ] **Step 2:** RED. **Step 3:** implement ui.rs + event loop + main. **Step 4:** gate GREEN. **Step 5:** commit `feat: pane UI with feed and mentions tabs`.

---

### Task 8: Plugin manifest, scripts, README, specs

**Files:**
- Create: `herdr-plugin.toml`, `herdr/install.sh`, `herdr/sidebar.sh`, `README.md`, `specs/overview.md`, `specs/config.md`, `specs/pane.md`, `specs/slack-host.md`

- [ ] **Step 1:** Adapt reviewr's `herdr-plugin.toml` + `herdr/install.sh` + `herdr/sidebar.sh`: id `dcieslak19973.slackr`, name `slackr`, pane entrypoint `feed` (title `slack`, placement split), actions toggle/open/close, REPO `dcieslak19973/herdr-slackr`, binary `herdr-slackr`, musl triples on Linux, `min_herdr_version = "0.7.0"`, platforms macos+linux. NO `[[events]]` block (spec: no auto-open).
- [ ] **Step 2:** README in reviewr's voice: what it is, Slack app setup (the exact scopes + user-event subscriptions + Socket Mode + app token from the spec §Constraints — this section is the user's checklist for the corporate approval request), install, tokens.toml + env setup, config reference, controls table, the manual smoke checklist (spec §Testing: run with real tokens → see a live message → kill network → watch "socket unavailable — polling" → restore → watch it recover), limitations (read-only, no persistence, nav-label spike result placeholder to fill in Task 9).
- [ ] **Step 3:** Four specs in the house voice: overview (invariants: read-only, no Slack state mutation, tokens never surfaced), config (key contracts), pane (tabs/keys/markers/degraded states), slack-host (Socket Mode lifecycle, ack contract, URL rotation, fallback semantics, REST methods + scopes table).
- [ ] **Step 4:** Guard: full gate still green (docs + scripts only; shellcheck sidebar.sh/install.sh if available, else careful read). **Step 5:** commit `docs: manifest, scripts, README, specs`.

---

### Task 9: E2E verification, push, release readiness

- [ ] **Step 1:** Full gate + `cargo build --release`. Record final test count.
- [ ] **Step 2:** Offline smoke on Windows (no Slack): binary with no tokens shows the remedy screen content path (drive via the render test if the TUI can't run headless; else `--version` + degraded checks). Verify `install.sh` target-triple mapping against the CI matrix names.
- [ ] **Step 3:** Push main, verify both CI jobs green (`gh run watch`).
- [ ] **Step 4:** Report: CI link, test count, and what CANNOT be verified without the user's Slack app (live socket, event subscriptions, nav-title spike) — these are the user's smoke checklist, gated on corporate app approval. Tag v0.1.0 only after the user's live smoke passes.

---

## Self-Review Notes

- **Spec coverage:** scaffold+CI/staticness (T1), tokens+config incl. 0600 (T2), model+entities+mention detection (T3), REST+429+error mapping (T4), socket worker+ack+backoff+URL rotation (T5), feed state+fallback dedup+divider+threads (T6), UI+keys+degraded+OSC spike (T7), manifest+scopes checklist+specs (T8), e2e+live-smoke handoff (T9). Non-goals held: no posting, no persistence, no tokio, no Enterprise Grid.
- **Type consistency:** `SocketEvent` (T5) consumed by `App::apply` (T6) and the loop (T7); `Rest`/`RestError` (T4) consumed by T6/T7; `PluginConfig`/`Tokens` (T2) by T6/T7; `ts_cmp`/`resolve`/`is_mention` (T3) by T6.
- **Judgment latitude:** exact Slack fixture field spellings are verifiable details (same rule as the forge plan); the policies (what acks, what folds to Down, what counts as a mention) are fixed here.
