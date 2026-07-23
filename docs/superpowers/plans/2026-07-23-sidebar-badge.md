# Sidebar Badge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put the unread-mention count on slackr's sidebar row via herdr's `pane report-metadata` CLI (herdr ≥ 0.7.4), degrading silently on older herdr.

**Architecture:** A new `src/herdr_meta.rs` module owns everything herdr-metadata: a pure argv builder, and a `Reporter` that gates on `(unread, health)` change, spawns the herdr CLI fire-and-forget, and latches disabled on first failure. The event loop calls it from the existing `dirty` block beside the kept OSC 0 title write. Spec: `docs/superpowers/specs/2026-07-23-sidebar-badge-design.md`.

**Tech Stack:** Rust (edition 2024), std only — `std::process::Command` + one spawned thread per report. No new dependencies.

## Global Constraints

- `#![forbid(unsafe_code)]` — crate-wide, already in force (spec invariant O5).
- No new crates in `Cargo.toml`.
- `min_herdr_version` stays `"0.7.0"` in `herdr-plugin.toml` (design decision 2).
- CLI syntax (verified against herdr 0.7.5 CLI reference + local 0.7.1 `herdr pane --help`):
  `herdr pane report-metadata <pane_id> --source ID [--title TEXT] [--token NAME=VALUE]…`
- Exact strings: source = `plugin:dcieslak19973.slackr`; title = `slack (3)` when unread > 0, `slack` when 0; tokens `slack_mentions=<n>` and `slack_link=<live|polling|lossy>`.
- The OSC 0 escape (`ui::nav_title`) is KEPT, unchanged (design decision 3).
- Never a status-bar message or `Blocked` state from reporter failure — plugin log only.
- Gates before every commit: `cargo test` all green, `cargo clippy --all-targets` clean, `cargo fmt`.
- House style: heavy `//!`/`///` doc comments that cite the spec (see `src/log.rs` for tone); tests in a `mod tests` at the bottom of the same file, descriptive snake_case names.

---

### Task 1: `herdr_meta` module — `LinkHealth` and the argv builder

**Files:**
- Create: `src/herdr_meta.rs`
- Modify: `src/lib.rs:8-23` (module list)
- Test: `mod tests` inside `src/herdr_meta.rs`

**Interfaces:**
- Consumes: nothing from this feature (first task).
- Produces: `pub enum LinkHealth { Live, Polling, Lossy }` (derives `Clone, Copy, Debug, PartialEq, Eq`); `pub fn argv(pane_id: &str, unread: usize, health: LinkHealth) -> Vec<String>`. Task 2 builds `Reporter` in this same file; Task 3 matches on `LinkHealth` variants in `lib.rs`.

- [ ] **Step 1: Register the module and write the failing tests**

In `src/lib.rs`, add to the module list (alphabetical, after `pub mod entities;`):

```rust
pub mod herdr_meta;
```

Create `src/herdr_meta.rs`:

```rust
//! Sidebar badge: report the unread-mention count onto this pane's herdr sidebar row via
//! `herdr pane report-metadata` (herdr >= 0.7.4). See
//! `docs/superpowers/specs/2026-07-23-sidebar-badge-design.md` and `specs/pane.md`
//! §Nav presence. On older herdr the call fails once, logs once, and the reporter stays
//! silent for the rest of the session — the badge is decoration, never function.

/// How the pane is currently receiving Slack messages, as reported in the `slack_link`
/// sidebar token. Derived by the event loop from state it already tracks (`lib.rs`):
/// poll-only mode or a down socket → `Polling`; a connected-but-proven-silent socket
/// (spec F17's `socket_lossy`) → `Lossy`; otherwise `Live`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkHealth {
    Live,
    Polling,
    Lossy,
}

impl LinkHealth {
    fn token(self) -> &'static str {
        match self {
            LinkHealth::Live => "live",
            LinkHealth::Polling => "polling",
            LinkHealth::Lossy => "lossy",
        }
    }
}

/// The `--source` identity every report carries, per the socket-api convention for
/// plugin-owned metadata.
const SOURCE: &str = "plugin:dcieslak19973.slackr";

/// Build the herdr CLI argv (everything after the binary name) for one metadata report.
/// Syntax verified against the herdr 0.7.5 CLI reference: `pane report-metadata <pane_id>
/// --source ID --title TEXT --token NAME=VALUE --token NAME=VALUE`. The title mirrors
/// `ui::nav_title`'s text (`slack (n)`, bare `slack` when read up); the tokens serve users
/// who render `$slack_mentions` / `$slack_link` in a custom sidebar row layout.
#[must_use]
pub fn argv(pane_id: &str, unread: usize, health: LinkHealth) -> Vec<String> {
    let title =
        if unread > 0 { format!("slack ({unread})") } else { "slack".to_string() };
    vec![
        "pane".to_string(),
        "report-metadata".to_string(),
        pane_id.to_string(),
        "--source".to_string(),
        SOURCE.to_string(),
        "--title".to_string(),
        title,
        "--token".to_string(),
        format!("slack_mentions={unread}"),
        "--token".to_string(),
        format!("slack_link={}", health.token()),
    ]
}

#[cfg(test)]
mod tests {
    use super::{LinkHealth, argv};

    #[test]
    fn argv_builds_the_full_report_metadata_call() {
        assert_eq!(
            argv("w1:p3", 3, LinkHealth::Live),
            [
                "pane",
                "report-metadata",
                "w1:p3",
                "--source",
                "plugin:dcieslak19973.slackr",
                "--title",
                "slack (3)",
                "--token",
                "slack_mentions=3",
                "--token",
                "slack_link=live",
            ]
        );
    }

    #[test]
    fn argv_zero_unread_uses_the_bare_title_and_a_zero_token() {
        let args = argv("w1:p3", 0, LinkHealth::Polling);
        assert!(args.contains(&"slack".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("slack (")));
        assert!(args.contains(&"slack_mentions=0".to_string()));
        assert!(args.contains(&"slack_link=polling".to_string()));
    }

    #[test]
    fn link_health_tokens_cover_all_three_states() {
        assert_eq!(argv("p", 1, LinkHealth::Lossy).last().unwrap(), "slack_link=lossy");
        assert_eq!(argv("p", 1, LinkHealth::Live).last().unwrap(), "slack_link=live");
        assert_eq!(
            argv("p", 1, LinkHealth::Polling).last().unwrap(),
            "slack_link=polling"
        );
    }
}
```

(Write the tests first if you prefer strict red-green inside the file; the module must exist for `lib.rs` to compile, so file-level TDD here means: write the file with `todo!()` bodies, see tests fail, then fill them. The shape above is the finished state.)

- [ ] **Step 2: Run the tests, expect them to pass (or fail first with `todo!()` bodies)**

Run: `cargo test herdr_meta`
Expected: `3 passed` (test names above).

- [ ] **Step 3: Full gates**

Run: `cargo test` → all green. `cargo clippy --all-targets` → no new warnings. `cargo fmt`.

- [ ] **Step 4: Commit**

```bash
git add src/herdr_meta.rs src/lib.rs
git commit -m "feat: herdr_meta argv builder for pane report-metadata"
```

---

### Task 2: `Reporter` — change gating, fire-and-forget spawn, failure latch

**Files:**
- Modify: `src/herdr_meta.rs` (append below `argv`)
- Test: extend `mod tests` in `src/herdr_meta.rs`

**Interfaces:**
- Consumes: `argv(pane_id, unread, health)` and `LinkHealth` from Task 1.
- Produces: `pub struct Reporter` with `pub fn from_env() -> Reporter`, `pub fn new(pane_id: Option<String>) -> Reporter`, `pub fn report(&mut self, unread: usize, health: LinkHealth)`. Task 3 calls `Reporter::from_env()` once and `report(...)` per dirty frame.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/herdr_meta.rs`:

```rust
    use super::Reporter;

    #[test]
    fn reporter_without_pane_id_never_produces_a_call() {
        let mut r = Reporter::new(None);
        assert_eq!(r.due(3, LinkHealth::Live), None);
        assert_eq!(r.due(4, LinkHealth::Polling), None);
    }

    #[test]
    fn reporter_fires_on_first_and_changed_pairs_only() {
        let mut r = Reporter::new(Some("w1:p3".to_string()));
        assert!(r.due(0, LinkHealth::Live).is_some(), "first report always fires");
        assert_eq!(r.due(0, LinkHealth::Live), None, "unchanged pair is a no-op");
        assert!(r.due(1, LinkHealth::Live).is_some(), "unread change fires");
        assert!(r.due(1, LinkHealth::Polling).is_some(), "health change fires");
        assert_eq!(r.due(1, LinkHealth::Polling), None);
    }

    #[test]
    fn reporter_failure_latch_disables_all_further_calls() {
        let mut r = Reporter::new(Some("w1:p3".to_string()));
        assert!(r.due(1, LinkHealth::Live).is_some());
        r.failed.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(r.due(2, LinkHealth::Live), None);
        assert!(r.logged, "first disabled call records the one-time log");
        assert_eq!(r.due(3, LinkHealth::Lossy), None);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test herdr_meta`
Expected: compile FAIL — `Reporter` not defined.

- [ ] **Step 3: Implement `Reporter`**

Append to `src/herdr_meta.rs` (above `mod tests`), and add the imports at the top of the file:

```rust
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
```

```rust
/// Reports `(unread, health)` onto this pane's sidebar row, at most once per change.
///
/// Failure latch (spec §Error handling): the CLI thread sets `failed` on any non-success
/// (old herdr rejecting `--token`, missing binary, no server); the next `report` call then
/// writes one plugin-log line and the reporter stays disabled for the session — the
/// plausible causes don't heal mid-run, and retries would only spam a shared machine's log.
pub struct Reporter {
    pane_id: Option<String>,
    last: Option<(usize, LinkHealth)>,
    failed: Arc<AtomicBool>,
    logged: bool,
}

impl Reporter {
    /// A reporter for the pane named by `$HERDR_PANE_ID`; permanently inert when unset
    /// (standalone/CLI runs outside a herdr pane).
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(std::env::var("HERDR_PANE_ID").ok())
    }

    #[must_use]
    pub fn new(pane_id: Option<String>) -> Self {
        Self { pane_id, last: None, failed: Arc::new(AtomicBool::new(false)), logged: false }
    }

    /// The gating half of [`report`](Self::report), separated so tests never spawn a
    /// subprocess: `Some(argv)` exactly when a report should fire — a pane id exists, the
    /// latch is clear, and `(unread, health)` differs from the last fired pair (seeded
    /// `None`, so the first call after startup always fires and labels the row).
    fn due(&mut self, unread: usize, health: LinkHealth) -> Option<Vec<String>> {
        let pane_id = self.pane_id.as_deref()?;
        if self.failed.load(Ordering::Acquire) {
            if !self.logged {
                self.logged = true;
                crate::logln!(
                    "sidebar badge: herdr pane report-metadata failed — disabled for \
                     this session (needs herdr >= 0.7.4)"
                );
            }
            return None;
        }
        if self.last == Some((unread, health)) {
            return None;
        }
        self.last = Some((unread, health));
        Some(argv(pane_id, unread, health))
    }

    /// Report one `(unread, health)` observation. No-op unless [`due`](Self::due) says
    /// otherwise; the CLI call runs on a detached thread so the event loop never blocks on
    /// a subprocess (spec §Design — fire-and-forget).
    pub fn report(&mut self, unread: usize, health: LinkHealth) {
        if let Some(args) = self.due(unread, health) {
            let failed = Arc::clone(&self.failed);
            std::thread::spawn(move || {
                let bin = std::env::var("HERDR_BIN_PATH")
                    .unwrap_or_else(|_| "herdr".to_string());
                let ok = Command::new(bin)
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if !ok {
                    failed.store(true, Ordering::Release);
                }
            });
        }
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test herdr_meta`
Expected: `6 passed`.

- [ ] **Step 5: Full gates**

Run: `cargo test`, `cargo clippy --all-targets`, `cargo fmt`.

- [ ] **Step 6: Commit**

```bash
git add src/herdr_meta.rs
git commit -m "feat: sidebar-badge Reporter with change gating and failure latch"
```

---

### Task 3: Event-loop wiring

**Files:**
- Modify: `src/lib.rs` (`event_loop`, two insertions)
- Modify: `src/ui.rs:414-418` (`nav_title` doc comment only)

**Interfaces:**
- Consumes: `herdr_meta::{Reporter, LinkHealth}` from Tasks 1–2; existing loop state `poll_only` (param), `app.polling`, `socket_lossy`, `unread`.
- Produces: nothing new — behavior only.

- [ ] **Step 1: Construct the reporter at loop setup**

In `src/lib.rs`, `event_loop`, directly after `let mut last_unread = app.unread_mentions();` (currently line ~194), insert:

```rust
    // Sidebar badge (spec `2026-07-23-sidebar-badge-design.md`): mirrors the OSC 0 title
    // below onto the pane's herdr sidebar row via `pane report-metadata`. Self-disables
    // after one failure (herdr < 0.7.4), so the OSC 0 escape stays as the only fallback.
    let mut reporter = herdr_meta::Reporter::from_env();
```

`lib.rs` is the crate root, so `herdr_meta::Reporter` resolves with no `use`; only the enum needs importing. Add to the existing `use crate::app::{App, Tab};` block:

```rust
use crate::herdr_meta::LinkHealth;
```

- [ ] **Step 2: Report from the dirty block**

Inside `if dirty { ... }` (currently `src/lib.rs:331-337`), the block reads:

```rust
        if dirty {
            let unread = app.unread_mentions();
            if unread != last_unread {
                let _ = write!(io::stdout(), "{}", ui::nav_title(unread));
                let _ = io::stdout().flush();
                last_unread = unread;
            }
```

Immediately after that inner `if` (before `terminal.draw`), insert:

```rust
            // `Polling` outranks `Lossy`: `socket_lossy` can only be set while the socket
            // is nominally up (`!app.polling` gates the safety poll), so the two are
            // mutually exclusive in practice; the order here just makes that explicit.
            let health = if poll_only || app.polling {
                LinkHealth::Polling
            } else if socket_lossy {
                LinkHealth::Lossy
            } else {
                LinkHealth::Live
            };
            reporter.report(unread, health);
```

(`reporter.report` self-gates on the changed pair — no `last_unread`-style tracking needed here, and the first dirty frame after startup fires the initial row-labeling report.)

- [ ] **Step 3: Update `nav_title`'s doc comment**

In `src/ui.rs`, replace the doc comment on `nav_title` (lines 414-418) with:

```rust
/// The OSC 0 terminal-title escape naming the unread mention count (spec §Nav presence).
/// The event loop emits this to stdout whenever `App::unread_mentions()` changes — kept as
/// the only badge path for herdr < 0.7.4, alongside `herdr_meta::Reporter`'s
/// `pane report-metadata` call which owns the sidebar row on newer herdr.
```

- [ ] **Step 4: Full gates**

Run: `cargo test` → all green (no behavior change any existing test observes; `HERDR_PANE_ID` is unset under `cargo test`, so the reporter is inert). `cargo clippy --all-targets` clean. `cargo fmt`.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/ui.rs
git commit -m "feat: report sidebar badge metadata from the event loop"
```

---

### Task 4: Docs, specs, manifest, changelog

**Files:**
- Modify: `README.md` (new section after `### Theme`, Limitations bullet, smoke checklist)
- Modify: `specs/pane.md:174-176` (§Nav presence)
- Modify: `specs/overview.md` (Scope list, Non-goals line 43)
- Modify: `herdr-plugin.toml:51-53` (trailing comment)
- Modify: `CHANGELOG.md` (`## [Unreleased]`)

- [ ] **Step 1: README — new `## Sidebar badge` section**

Insert after the `### Theme` subsection ends (immediately before `## Navigation`, currently line 270):

```markdown
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
```

- [ ] **Step 2: README — replace the Limitations bullet**

Replace the whole `- **No native nav badge, unverified.** …` bullet (currently lines 536-541) with:

```markdown
- **Sidebar badge needs herdr ≥ 0.7.4.** On older herdr the `pane report-metadata` call fails
  once, logs once, and stays off for the session (see [Sidebar badge](#sidebar-badge)); the
  OSC 0 terminal-title escape (`slack (n)`) remains the only — unverified — nav signal there,
  and the pane's own tab-bar count the only reliable one.
```

- [ ] **Step 3: README — smoke checklist item**

Append to `## Manual smoke checklist` (after the existing numbered items), renumbering if the list style requires:

```markdown
N. **Sidebar badge** (herdr ≥ 0.7.5). Receive a DM: the pane's sidebar row should read
   `slack (1)` within a tick; mark it read (`Enter` on the Mentions row) and the row returns
   to `slack`. On herdr < 0.7.4: exactly one `sidebar badge: …` line in the plugin log
   (`herdr plugin log list --plugin dcieslak19973.slackr`), no other visible behavior.
```

- [ ] **Step 4: `specs/pane.md` — rewrite §Nav presence**

Replace the section body (currently lines 174-176) with:

```markdown
## Nav presence

| #  | Always true                                                                                                     |
| -- | --------------------------------------------------------------------------------------------------------------- |
| N1 | On herdr ≥ 0.7.4, the pane reports its sidebar row title (`slack (n)` / bare `slack` at zero) and the tokens `slack_mentions` + `slack_link` (`live`/`polling`/`lossy`) via `herdr pane report-metadata`, source `plugin:dcieslak19973.slackr` — at most one call per `(unread, link)` change, spawned fire-and-forget, never blocking the event loop (`herdr_meta::Reporter`). |
| N2 | The first failed report writes one plugin-log line and disables the reporter for the session. Failure never surfaces in the status bar and never triggers `Blocked` (O4): the badge is decoration, not function. Unset `$HERDR_PANE_ID` disables it from the start. |
| N3 | The OSC 0 terminal-title escape (`slack (n)` on every unread change) is kept as the pre-0.7.4 fallback; whether any herdr version renders it in the nav remains unverified. |
```

- [ ] **Step 5: `specs/overview.md` — scope and non-goals**

In `## Scope`, append:

```markdown
- Sidebar badge: unread count and link health onto the pane's herdr sidebar row (`pane.md` §Nav presence).
```

Replace the Non-goals line (currently line 43) `- Native herdr nav/badge integration beyond an unverified terminal-title spike (see `pane.md`).` with:

```markdown
- Deeper herdr nav integration than the pane's own sidebar row — `agent.view.*` ordering/filtering, custom sidebar rows or sections (`pane.md` §Nav presence covers what *is* in scope).
```

Bump the frontmatter `Last edited:` date to `2026-07-23` in both edited spec files.

- [ ] **Step 6: `herdr-plugin.toml` — trailing comment**

Replace lines 51-53 (the `# No [[events]] block …` comment) with:

```toml
# No [[events]] block: unlike reviewr, slackr never auto-opens — the user opens the feed once
# per session via the toggle action. Nav presence is the pane's own sidebar row: the binary
# reports its title and $slack_mentions/$slack_link tokens via `pane report-metadata` on
# herdr >= 0.7.4 (specs/pane.md §Nav presence); deeper nav integration stays out of scope.
```

- [ ] **Step 7: CHANGELOG**

Under `## [Unreleased]`:

```markdown
### Added
- **Sidebar badge.** On herdr ≥ 0.7.4 the pane labels its own sidebar row `slack (n)` with
  the unread mention count via `pane report-metadata`, and publishes `$slack_mentions` /
  `$slack_link` metadata tokens for custom row layouts (README §Sidebar badge). Older herdr:
  one plugin-log line, then silence — the OSC 0 terminal-title fallback stays.
```

- [ ] **Step 8: Gates and commit**

Run: `cargo test` (docs shouldn't break anything; belt and braces), `cargo fmt --check`.

```bash
git add README.md specs/pane.md specs/overview.md herdr-plugin.toml CHANGELOG.md
git commit -m "docs: sidebar badge — README, pane/overview specs, manifest note, changelog"
```

---

### Task 5: Live smoke (deferred — needs herdr ≥ 0.7.5)

Not executable in this session; recorded so the branch/PR text carries it. From the spec:

1. Badge renders: on a herdr ≥ 0.7.5 install, a new DM flips the sidebar row to `slack (1)`;
   marking it read returns it to `slack`; `$slack_mentions`/`$slack_link` render once added
   to `[ui.sidebar.agents]` rows.
2. Old-herdr degradation: on 0.7.1, exactly one `sidebar badge:` plugin-log line, nothing
   else visible.

The PR description must list both as unchecked boxes, mirroring how F17/F18 deferred their
live checks to Dan's work install.
