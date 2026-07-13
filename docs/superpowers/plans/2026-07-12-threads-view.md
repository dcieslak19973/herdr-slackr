# Threads View + Row Colors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Real thread support everywhere (metadata-driven markers, bounded reply-refresh in polling mode), a threads-only view toggled with `t`, and colored row segments.

**Architecture:** Per `docs/superpowers/specs/2026-07-12-threads-and-colors-design.md`. Data layer in `model.rs`/`rest.rs`/`app.rs` (Task 1); view + colors in `app.rs` (row projection) and `ui.rs`/`lib.rs` (Task 2); docs + 0.1.3 (Task 3).

**Tech Stack:** Existing crate, closed dep list.

## Global Constraints

- Spec above is the contract; all prior invariants bind (read-only, token hygiene, 8-call tick budget total, no tokio).
- House gate per commit: clippy pedantic `-D warnings`; `cargo test --all-features` (227 green today); `cargo fmt --all` + discard CRLF-only rewrites. Dense doc comments. Branch `threads-view`. Trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

---

### Task 1: Thread metadata + polling reply-refresh

**Files:**
- Modify: `src/model.rs` (`Message.reply_count: Option<u32>` — thread through every constructor/fixture; mechanical), `src/rest.rs` (parse `reply_count` in `parse_messages`; `replies()` gains `oldest: Option<&str>` like `history`), `src/app.rs` (marker count = `max(root.reply_count.unwrap_or(0), local_replies)`; active-thread tracking: expanded set ∪ roots where `reply_count > local replies`; `poll_tick_at` splits the 8-slot budget — up to 2 slots rotate over active threads via a second round-robin cursor, issuing `replies(conv, thread_ts, oldest=newest known reply ts)`; fetched replies go through `upsert_new`), `src/socket.rs` only if root `message_changed` events carry `reply_count` (parse when present — verify the envelope shape; else document why not).

**Interfaces:**
- Produces: `Message.reply_count`; `App::active_threads() -> Vec<(conv_id, thread_ts)>`-style seam (pure, tested); the split-budget scheduling as pure fns extending the existing `next_batch` policy family.

- [ ] TDD: marker-count max() cases (metadata only / local only / both); active-thread selection (expanded, count-gap, neither); 2-slot rotation math incl. zero active threads (all 8 slots go to conversations — the budget is never wasted); `parse_messages` reply_count fixture; `replies` oldest arg threading. Existing poll tests updated for the split budget asserting MEANING. Full gate. Commit `feat: thread metadata and polling reply refresh`.

---

### Task 2: Threads view + row colors

**Files:**
- Modify: `src/app.rs` (`FeedView { Timeline, Threads }` state + `toggle_view()`; `thread_rows()` projection: threads only, ordered by latest activity desc, root + nested local replies always shown, Enter on root = existing expand/refresh path; `feed_rows` untouched; `current_ids`/`resync_cursor` extended for the new projection), `src/ui.rs` (row rendering as styled spans: conv label = palette blue/sapphire field, author = green, time/markers = muted overlay, text = default fg — read src/theme.rs for the exact field names; tab bar mode marker; footer gains `t`), `src/lib.rs` (`t` key → `app.toggle_view()`, Feed tab only), `README.md` Controls row for `t` (Task 3 owns the rest of docs).
- Test: pure `thread_rows` ordering/nesting/exclusion; render tests: threads-view snapshot, per-segment color assertions (the render suite already asserts colors — same pattern), `t` toggle flips the projection, timeline unchanged.

**Interfaces:**
- Consumes Task 1's `reply_count`/marker logic. Selection identity: thread-view rows reuse existing `SelKind` (root rows = Message kind with the root key; reply rows = Message kind with the reply key).

- [ ] TDD; full gate; commit `feat: threads view and colored row segments`.

---

### Task 3: Docs + 0.1.3 + release prep

- Modify: `README.md` (threads view section: what `t` shows, exclusion semantics, polling reply-refresh note; colors mention in the theme section), `specs/pane.md` (threads-view rows/keys/selection + colors), `specs/slack-host.md` (budget split F-row), `CHANGELOG.md` ([0.1.3]), versions → 0.1.3 (3 files). Docs must match landed code.
- [ ] Full gate; commit `docs: threads view and colors; release 0.1.3 prep`.

---

## Self-Review Notes
- Spec §1→T1, §2→T1, §3→T2, §4→T2, docs→T3. Non-goals held (no CLI change, no read-tracking, budget stays 8).
- Type consistency: `reply_count` named identically in T1/T2; `FeedView`/`thread_rows`/`toggle_view` defined T2 and consumed by lib.rs there.
