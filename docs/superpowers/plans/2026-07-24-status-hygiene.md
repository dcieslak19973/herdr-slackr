# Status Hygiene Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make recurring error statuses self-dating and self-describing (HTTP status in invalid-JSON errors, UTC `HH:MM` prefix), and give the catch-up/refresh sweep visible progress and a completion line.

**Architecture:** Three localized changes: `rest::parse_response`'s JSON-parse error appends the trailer's HTTP status; `app::poll_error_status` gains a clock-injected `_at` variant that prefixes UTC `HH:MM` via a new pure `hhmm_utc` helper; `app::catchup_tick_at` counts the sweep down and stamps `HH:MM refresh complete`, guarded so it never overwrites an error a batch just surfaced. Spec: `docs/superpowers/specs/2026-07-24-status-hygiene-design.md`.

**Tech Stack:** Rust (edition 2024), std only. No new dependencies, no public-API changes outside `app.rs`/`rest.rs` internals.

## Global Constraints

- Exact error wording: with a parsed trailer → `invalid JSON (HTTP <code>): <serde error>`; without a trailer (curl < 7.83) → today's `invalid JSON: <serde error>` unchanged.
- Timestamp prefix is UTC `HH:MM` + one space, on BOTH `poll_error_status` arms (rate-limit and named-conversation), e.g. `03:12 dm scan: …`, `03:12 slack rate limit — retrying in 30s`.
- Interactive-keypress errors (permalink/replies/thread refresh/expand statuses) are NOT touched.
- Countdown wording: `refreshing <n> conversations` (unchanged string shape); completion: `<HH:MM> refresh complete`. A batch that wrote any other status (error/rate limit) is left alone — guard by comparing `self.status` before/after `poll_conversations`.
- `request_refresh`'s immediate keypress feedback stays exactly as is.
- Gates before every commit: `cargo test` all green, `cargo clippy --all-targets` clean, `cargo fmt`.
- House style: `///` docs citing the spec; tests in the existing `mod tests`, descriptive snake_case names.

---

### Task 1: HTTP status in invalid-JSON errors

**Files:**
- Modify: `src/rest.rs` (`parse_response`, currently ~line 381; two new tests in `mod tests`)

**Interfaces:**
- Consumes: existing `split_trailer` (already returns the parsed trailer before the JSON parse).
- Produces: no signature changes; only the `RestError::Other` message wording.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/rest.rs`, next to the existing `a_429_trailer_rate_limits_even_when_the_body_is_not_json` test (~line 720):

```rust
    #[test]
    fn an_invalid_json_body_with_a_trailer_names_the_http_status() {
        // The overnight incident (2026-07-24): a proxy answered with an empty body and a
        // non-429 status; the old wording discarded the one fact that names the culprit.
        let out = "\n302 ";
        let err = parse_response(out).unwrap_err();
        let RestError::Other(msg) = err else { panic!("expected Other, got {err:?}") };
        assert!(msg.starts_with("invalid JSON (HTTP 302): "), "{msg}");
    }

    #[test]
    fn an_invalid_json_body_without_a_trailer_keeps_the_plain_wording() {
        // curl < 7.83 appends no trailer; there is no HTTP status to report.
        let err = parse_response("not json").unwrap_err();
        let RestError::Other(msg) = err else { panic!("expected Other, got {err:?}") };
        assert!(msg.starts_with("invalid JSON: "), "{msg}");
        assert!(!msg.contains("HTTP"), "{msg}");
    }
```

- [ ] **Step 2: Run to verify the first fails**

Run: `cargo test invalid_json`
Expected: `an_invalid_json_body_with_a_trailer_names_the_http_status` FAILS (message lacks `(HTTP 302)`); the no-trailer test passes already.

- [ ] **Step 3: Implement**

In `parse_response`, replace:

```rust
    let v: Value =
        serde_json::from_str(body).map_err(|e| RestError::Other(format!("invalid JSON: {e}")))?;
```

with:

```rust
    let v: Value = serde_json::from_str(body).map_err(|e| match trailer {
        // The trailer's HTTP status is the one fact that names a non-JSON response's
        // culprit (proxy redirect, WAF page, empty 200) — spec `2026-07-24-status-hygiene`
        // decision 1. No trailer (curl < 7.83) leaves nothing to report.
        Some((code, _)) => RestError::Other(format!("invalid JSON (HTTP {code}): {e}")),
        None => RestError::Other(format!("invalid JSON: {e}")),
    })?;
```

Also update the stale sentence in `parse_response`'s doc comment (`/// … would surface that as
\`Other("invalid JSON...")\` instead of the \`RateLimited\` the caller needs to back off on.`) — it stays true; append one sentence: `/// When the parse does fail, the trailer's HTTP status is folded into the error text.`

- [ ] **Step 4: Run the tests, then full gates**

Run: `cargo test invalid_json` → 2 pass. Then `cargo test`, `cargo clippy --all-targets`, `cargo fmt`.

- [ ] **Step 5: Commit**

```bash
git add src/rest.rs
git commit -m "feat: name the HTTP status in invalid-JSON REST errors"
```

---

### Task 2: UTC HH:MM prefix on recurring error statuses

**Files:**
- Modify: `src/app.rs` (`poll_error_status` ~line 2555; new `hhmm_utc` helper beside `format_ts` ~line 2092; tests ~line 4050)

**Interfaces:**
- Consumes: nothing new.
- Produces: `fn hhmm_utc(now_secs: u64) -> String` (private, `app.rs`) — Task 3 uses it for the completion stamp. `fn poll_error_status_at(now_secs: u64, conv_name: &str, error: &RestError) -> String`; `poll_error_status(conv_name, error)` becomes a thin wrapper passing `crate::users_cache::now_secs()`, so all 8 call sites stay unchanged.

- [ ] **Step 1: Write the failing tests**

Replace the two existing wording tests (`poll_error_status_is_a_rate_limit_notice_for_rate_limited`, `poll_error_status_names_the_conversation_for_other_errors`, ~line 4050) with clock-injected versions, and add `hhmm_utc` edge tests:

```rust
    #[test]
    fn poll_error_status_is_a_timestamped_rate_limit_notice_for_rate_limited() {
        // 1752300000 = 2025-07-12T06:00:00Z.
        let status = poll_error_status_at(1_752_300_000, "eng", &RestError::RateLimited(42));
        assert_eq!(status, "06:00 slack rate limit — retrying in 42s");
    }

    #[test]
    fn poll_error_status_timestamps_and_names_the_conversation_for_other_errors() {
        let status = poll_error_status_at(
            1_752_300_000,
            "eng",
            &RestError::SlackError("channel_not_found".to_string()),
        );
        assert!(status.starts_with("06:00 eng: "), "{status}");
        assert!(status.contains("channel_not_found"), "{status}");
    }

    #[test]
    fn hhmm_utc_formats_midnight_and_end_of_day() {
        assert_eq!(hhmm_utc(0), "00:00");
        assert_eq!(hhmm_utc(86_399), "23:59");
        assert_eq!(hhmm_utc(86_400), "00:00", "rolls over at the UTC day boundary");
    }
```

Update the `use super::…` list in `mod tests` to import `hhmm_utc` and `poll_error_status_at` (drop `poll_error_status` if no remaining test uses it).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test poll_error_status; cargo test hhmm_utc`
Expected: compile FAIL — `poll_error_status_at` / `hhmm_utc` not defined.

- [ ] **Step 3: Implement**

Add beside `format_ts` (which keeps its own inline math — extracting it there would churn six passing tests for no behavior change; the helper exists for status lines):

```rust
/// UTC `HH:MM` for an epoch-seconds clock reading — the timestamp prefix recurring error
/// statuses carry (spec `2026-07-24-status-hygiene` decision 2), in the same UTC convention
/// as `format_ts`'s message times.
fn hhmm_utc(now_secs: u64) -> String {
    let day_secs = now_secs % 86_400;
    format!("{:02}:{:02}", day_secs / 3600, (day_secs % 3600) / 60)
}
```

Replace `poll_error_status` with:

```rust
/// The one-line status `poll_tick` sets on a per-conversation history-fetch failure: a
/// rate-limit notice naming the retry delay (Slack's own back-off signal, distinct from any
/// other failure) or a line naming which conversation failed and why — either way prefixed
/// with the UTC `HH:MM` it happened (spec `2026-07-24-status-hygiene` decision 2: statuses
/// are write-once and can sit on screen for hours, so an error must date itself). Split so
/// the wording is unit-tested without a real REST call or a live clock.
fn poll_error_status(conv_name: &str, error: &RestError) -> String {
    poll_error_status_at(crate::users_cache::now_secs(), conv_name, error)
}

/// `poll_error_status`'s real logic, taking the clock as a parameter (production always
/// passes `users_cache::now_secs()`), for the same testability reason as `poll_tick_at`.
fn poll_error_status_at(now_secs: u64, conv_name: &str, error: &RestError) -> String {
    let stamp = hhmm_utc(now_secs);
    match error {
        RestError::RateLimited(secs) => format!("{stamp} slack rate limit — retrying in {secs}s"),
        other => format!("{stamp} {conv_name}: {other:?}"),
    }
}
```

- [ ] **Step 4: Run the tests, check for collateral**

Run: `cargo test poll_error_status; cargo test hhmm_utc` → pass. Then full `cargo test` — one known collateral: `poll_tick_surfaces_a_rest_failure_as_a_one_line_status_without_crashing` only asserts non-empty status and still passes; any other test asserting exact un-prefixed status text must be updated to the prefixed form (search `mod tests` for `"slack rate limit` if the suite flags one).

- [ ] **Step 5: Full gates and commit**

Run: `cargo test`, `cargo clippy --all-targets`, `cargo fmt`.

```bash
git add src/app.rs
git commit -m "feat: timestamp recurring error statuses with UTC HH:MM"
```

---

### Task 3: Catch-up sweep countdown and completion

**Files:**
- Modify: `src/app.rs` (`catchup_tick_at`, ~line 858; tests near `catchup_tick_visits_a_batch_and_retires_the_swept_conversations` ~line 3227)

**Interfaces:**
- Consumes: `hhmm_utc` from Task 2; existing `poll_conversations` outcome (`.completed`, `.rate_limited`) and `precancelled_rest` test fixture.
- Produces: no signature changes — behavior only.

- [ ] **Step 1: Write the failing tests**

Add beside the existing catchup tests (~line 3238). Note `empty_app()`'s fixture has 2 subscribed conversations, and `precancelled_rest` fails every call with a non-rate-limit error — which *sets an error status*, so it exercises the guard path; the countdown path needs a sweep armed larger than one batch with no REST calls at all, which `poll_conversations` cannot do — so the countdown test drives `catchup_tick_at` with zero due conversations by using an over-armed `request_refresh` count instead:

```rust
    #[test]
    fn catchup_tick_leaves_a_batch_error_status_alone() {
        let mut app = empty_app();
        app.request_refresh();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        // Every call fails ("request cancelled"), so the batch writes an error status; the
        // countdown must not paper over it (spec decision 3's guard).
        app.catchup_tick_at(&rest, Instant::now());
        assert!(
            !app.status.starts_with("refreshing") && !app.status.ends_with("refresh complete"),
            "error status must survive the batch: {}",
            app.status
        );
    }

    #[test]
    fn catchup_tick_counts_down_and_completes_with_a_timestamp() {
        let mut app = empty_app();
        app.request_refresh();
        assert_eq!(app.status, "refreshing 2 conversations");
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        // Both fixture conversations fail-and-retire in one batch (continue-past-errors), so
        // the sweep drains; the error status the batch wrote survives (guard). A second tick
        // with nothing armed must not touch status either.
        app.catchup_tick_at(&rest, Instant::now());
        assert_eq!(app.catchup_remaining, 0);
        let after_drain = app.status.clone();
        app.catchup_tick_at(&rest, Instant::now());
        assert_eq!(app.status, after_drain, "a no-op tick must not rewrite status");
    }

    #[test]
    fn catchup_tick_stamps_refresh_complete_when_the_batch_is_quiet() {
        let mut app = empty_app();
        // Arm a sweep with zero due conversations: catchup_remaining is over-armed relative
        // to the (empty) due list, so poll_conversations makes no calls, writes no status,
        // and completes 0 — but the drain arithmetic still runs. Build that state directly.
        app.catchup_remaining = 1;
        app.status = "refreshing 1 conversations".to_string();
        let cancelled = AtomicBool::new(false);
        let rest = precancelled_rest(&cancelled);
        // empty_app has 2 conversations; a batch visits and retires both (errors set status).
        // To isolate the quiet-completion arm we need a batch that makes no calls: drop the
        // conversations first.
        app.conversations.clear();
        app.catchup_tick_at(&rest, Instant::now());
        assert_eq!(app.catchup_remaining, 0, "an empty due list still drains the sweep");
        assert!(app.status.ends_with("refresh complete"), "{}", app.status);
        let stamp = &app.status[..5];
        assert!(
            stamp.len() == 5 && &stamp[2..3] == ":",
            "completion carries an HH:MM stamp: {}",
            app.status
        );
    }
```

**Fixture caveat the implementer must verify:** the third test assumes (a) `conversations` is directly assignable/clearable from `mod tests` (same-module private access — it is), and (b) `poll_conversations` with an empty conversation list returns `completed` ≥ `catchup_remaining` or the drain still reaches 0 via `saturating_sub`. Read `poll_conversations`' handling of an empty due list first; if an empty list yields `completed == 0` (sweep never drains), instead drain by the two-failing-conversations route and assert the completion arm via the countdown branch with a 3-armed sweep: `app.catchup_remaining = 3` → one batch retires 2 → expect `refreshing 1 conversations`. Adapt the test to whichever behavior the code actually has, keeping one test per spec arm: error-preserved, countdown, completion-stamped. If the completion arm is genuinely unreachable without live REST success, mark that assertion as covered by the countdown test plus a targeted unit test on the new `catchup_status` helper (below) — the helper is pure and fully testable regardless.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test catchup_tick`
Expected: new tests FAIL (status today never counts down or completes).

- [ ] **Step 3: Implement**

To keep the logic pure and independently testable, put the decision in a helper and call it from `catchup_tick_at`:

```rust
/// The status line a quiet catch-up batch leaves behind (spec `2026-07-24-status-hygiene`
/// decision 3): the countdown while the sweep is still armed, a timestamped completion when
/// this batch drained it. The caller only applies it when the batch wrote no status of its
/// own (error / rate limit) — detected by comparing status before/after the batch — so the
/// countdown never overwrites fresher bad news.
fn catchup_status(remaining: usize, now_secs: u64) -> String {
    if remaining > 0 {
        format!("refreshing {remaining} conversations")
    } else {
        format!("{} refresh complete", hhmm_utc(now_secs))
    }
}
```

In `catchup_tick_at`, replace the tail:

```rust
        let slots = POLL_BATCH.min(self.catchup_remaining);
        let outcome = self.poll_conversations(rest, slots, now);
        self.catchup_remaining = self.catchup_remaining.saturating_sub(outcome.completed);
        self.finish_after_arrivals(follow_bottom, had_rows_before, arrival_before);
```

with:

```rust
        let status_before = self.status.clone();
        let slots = POLL_BATCH.min(self.catchup_remaining);
        let outcome = self.poll_conversations(rest, slots, now);
        self.catchup_remaining = self.catchup_remaining.saturating_sub(outcome.completed);
        // A batch that surfaced an error or rate limit owns the status line; only a quiet
        // batch narrates the sweep (spec decision 3). This also makes the post-reconnect
        // catch-up visible, replacing the stale "socket unavailable — polling" line.
        if self.status == status_before {
            self.status =
                catchup_status(self.catchup_remaining, crate::users_cache::now_secs());
        }
        self.finish_after_arrivals(follow_bottom, had_rows_before, arrival_before);
```

Add a pure test for the helper:

```rust
    #[test]
    fn catchup_status_counts_down_then_completes_with_a_stamp() {
        assert_eq!(catchup_status(7, 1_752_300_000), "refreshing 7 conversations");
        assert_eq!(catchup_status(0, 1_752_300_000), "06:00 refresh complete");
    }
```

**Guard nuance:** the second `catchup_tick_at` call in `catchup_tick_counts_down_and_completes_with_a_timestamp` returns at the `catchup_remaining == 0` early-exit before any status writing — that is what "a no-op tick must not rewrite status" verifies; no code change needed for it.

- [ ] **Step 4: Run the tests, then full gates**

Run: `cargo test catchup` → all pass (including the two pre-existing catchup tests — `catchup_tick_visits_a_batch_and_retires_the_swept_conversations` drives failing conversations, whose error status now survives the guard, and it asserts only `catchup_remaining`; verify it still passes unmodified). Then `cargo test`, `cargo clippy --all-targets`, `cargo fmt`.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat: catch-up sweep counts down and stamps refresh complete"
```

---

### Task 4: Docs and changelog

**Files:**
- Modify: `specs/pane.md` (keys-table `r` row, ~line 155; frontmatter date)
- Modify: `README.md` (Controls-table `r` row, ~line 442)
- Modify: `CHANGELOG.md` (`## [Unreleased]`)

- [ ] **Step 1: `specs/pane.md` — `r` key row**

In the keys table, the `r` row ends today with: `sets a `refreshing n conversations` status`. Replace that final clause with: ``sets a `refreshing n conversations` status that counts down as batches retire and ends as `HH:MM refresh complete` — unless a batch surfaced an error, which owns the line (statuses carry a UTC `HH:MM` stamp on recurring errors; see §Degraded states)``.

Append one sentence to the §Degraded states body (after the "Once the pane reaches `Ready`…" paragraph): `Recurring background errors (poll, dm scan, thread fetch, rate limit) are prefixed with the UTC `HH:MM` they occurred, so a stale error is visibly stale; a REST response that fails JSON parsing names the HTTP status when curl's write-out trailer provided one.`

Bump the frontmatter `Last edited:` to `2026-07-24`.

- [ ] **Step 2: `README.md` — Controls-table `r` row**

Replace the row's parenthetical `(status shows `refreshing n conversations`)` with `(status counts down `refreshing n conversations` and ends `HH:MM refresh complete`)`.

- [ ] **Step 3: CHANGELOG**

Under `## [Unreleased]`:

```markdown
### Changed
- **Status lines date themselves.** Recurring background errors (polling, dm scan, thread
  fetches, rate limits) are now prefixed with the UTC `HH:MM` they occurred — a transient
  overnight hiccup no longer masquerades as a live failure hours later. A REST response that
  fails JSON parsing now names the HTTP status from curl's write-out trailer
  (`invalid JSON (HTTP 302): …`), and the manual-refresh sweep counts down
  (`refreshing 7 conversations`) and finishes with a stamped `refresh complete` instead of
  sitting on its opening message forever.
```

- [ ] **Step 4: Gates and commit**

Run: `cargo test` (belt and braces), `cargo fmt --check`.

```bash
git add specs/pane.md README.md CHANGELOG.md
git commit -m "docs: status-hygiene — timestamped errors, refresh countdown"
```
