# Sidebar badge via herdr pane metadata — design

Date: 2026-07-23
Status: approved (brainstorm with Dan, 2026-07-23)

## Goal

Put the unread-mention count on slackr's own row in herdr's left sidebar, so the user sees
`slack (3)` at a glance without the feed pane focused. This replaces guesswork: the existing
"nav presence" spike (`ui::nav_title`, spec `specs/pane.md` §Nav presence) emits an OSC 0
terminal-title escape that was never verified to render anywhere.

## Background: what herdr now offers

herdr 0.7.4 (2026-07-15) added configurable sidebar row layouts with **custom metadata
tokens reported through the CLI and socket API**; 0.7.5 (2026-07-21) added per-token
fg/bold/dim styling and `agent.view.set/clear`. The reporting call is `pane.report_metadata`
(socket method; also exposed via the herdr CLI): params include `pane_id`, `source`,
`title`, `tokens` (string map; JSON null clears a token), and optional `ttl_ms`. `title`
changes the row's label with zero user config; `tokens` render as `$name` only where the
user's row-layout config places them.

Verified sources: herdr CHANGELOG (0.7.2–0.7.5), herdr.dev/docs/socket-api. The exact CLI
subcommand spelling is **unverified** — an implementation task pins it against a live 0.7.5
(`herdr --help`), falling back to the socket call only if the CLI turns out not to expose it.

## Decisions (from the brainstorm)

1. **Report both `title` and `tokens`.** `title` = the zero-config badge; tokens serve users
   who customize row layouts (styleable per-token on 0.7.5).
2. **Degrade silently on older herdr.** Dan's installs are 0.7.1-preview today. First failed
   report: one plugin-log line, then a session-long disabled latch. `min_herdr_version`
   stays `0.7.0`.
3. **Keep the OSC 0 spike** alongside the reporter — it may render on pre-0.7.4 herdr and
   costs nothing.
4. **Transport: shell out to the herdr CLI** (`$HERDR_BIN_PATH`, default `herdr`), matching
   herdr-reviewr's `src/herdr.rs` idiom. No direct socket client: the wire framing is
   unverified protocol surface, and report frequency (mention-count changes) never justifies
   it. `agent.view.*` is out of scope.

## Design

### Reporter module — `src/herdr_meta.rs`

One small module owning everything herdr-metadata:

- `pub enum LinkHealth { Live, Polling, Lossy }` — derived in the event loop from state it
  already tracks: `poll_only || app.polling` → `Polling`; `socket_lossy` → `Lossy`; else
  `Live` (see `lib.rs` run loop, `specs/slack-host.md` F17).
- `pub struct Reporter` holding the disabled latch and the last-reported `(unread, health)`
  pair. `Reporter::report(unread: usize, health: LinkHealth)` is a no-op when disabled or
  unchanged; otherwise it spawns the CLI call on a fire-and-forget thread (the event loop
  never blocks on a subprocess). The thread reports failure back through a shared
  `Arc<AtomicBool>`; once set, the latch logs one `crate::logln!` line
  (`sidebar badge: herdr pane report-metadata failed — disabled for this session (needs
  herdr >= 0.7.4)`) and suppresses all further attempts.
- Payload construction is a pure function (unit-testable, same style as `nav_title`):
  - `title`: `slack (3)` when unread > 0, `slack` when 0 — mirrors `nav_title`'s text.
  - `tokens`: `slack_mentions` = decimal count (`"0"` when read up), `slack_link` =
    `live` / `polling` / `lossy`.
  - `source`: `plugin:dcieslak19973.slackr`; `pane_id` from `$HERDR_PANE_ID`; no `ttl_ms`
    (pane metadata dies with the pane; the pair-change gate keeps reports rare).
- Missing `$HERDR_PANE_ID` disables the reporter at construction (standalone/CLI runs).

### Event-loop wiring — `src/lib.rs`

The existing `dirty` block already recomputes `unread_mentions()` and emits the OSC 0 title
on change (`lib.rs` ~332). The reporter call sits beside it: compute `LinkHealth`, call
`reporter.report(unread, health)`. The OSC 0 write stays exactly as is (decision 3). One
report also fires right after startup's first draw so the row is labeled before the first
mention arrives.

### Error handling

- CLI exit ≠ 0, spawn failure (binary missing), or missing pane id → the latch, one log
  line, silence. Never a status-bar message, never `Blocked` (pane invariant O4 territory —
  the badge is decoration, not function).
- No retry within a session: the plausible causes (old herdr, no socket) don't heal mid-run,
  and retrying against a shared work machine's plugin log would spam it.

### Docs — README

New "Sidebar badge" subsection: works out of the box on herdr ≥ 0.7.4 (row title shows
`slack (n)`); copy-paste row-layout snippet showing `$slack_mentions` / `$slack_link` with
0.7.5 per-token styling; note the silent no-op on older herdr. The Limitations entry about
the unverified terminal-title spike gets rewritten to describe the layered behavior.

### Spec and manifest updates (same change)

- `specs/pane.md` §Nav presence: rewritten — title/token reporting via `pane.report_metadata`
  (new numbered behavior rows), OSC 0 kept as the pre-0.7.4 fallback, silent-degrade rule.
- `specs/overview.md` Non-goals: the "Native herdr nav/badge integration" line is narrowed
  (badge is now in scope; deeper nav integration — `agent.view.*`, custom rows — stays out).
- `herdr-plugin.toml`: the trailing comment declaring nav integration out-of-scope is
  updated to point at the reporter.

## Testing

- Unit: payload/title/token formatting (pure function, table style like `nav_title`'s test);
  latch behavior (disabled after failure, no-op on unchanged pair, disabled without
  `HERDR_PANE_ID`) tested through the private `due()` gating seam — no subprocess in tests.
- Live smoke (deferred to a herdr ≥ 0.7.5 install — work, after upgrade), two checks:
  1. Badge renders: unread mentions change the sidebar row to `slack (n)`; tokens render
     once the row layout references them.
  2. Old-herdr degradation: on 0.7.1, at most one plugin-log line, no visible misbehavior.

## Out of scope

- `agent.view.set/clear` (sidebar ordering/filtering) — YAGNI until a real need.
- Direct socket-API client, TTL-based staleness, per-channel counts, reporting from
  `sidebar.sh` actions.
