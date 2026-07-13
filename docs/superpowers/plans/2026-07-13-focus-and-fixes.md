# DM Cap Fix, Allow-List, Focus Mode, Dated Timestamps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `dm_limit` never blocks new DMs in either delivery mode; `dm_allow` always-subscribes named DMs; a `f`-toggled Focus view surfaces only live, session-fresh messages matching the allow-list or focus keywords; timestamps show the date for prior days.

**Architecture:** Per `docs/superpowers/specs/2026-07-13-focus-and-fixes-design.md`. Task 1 = config keys + `resolve_channels` allow-list (§2). Task 2 = the out-of-cap poll detection path (§1). Task 3 = Focus mode (§3). Task 4 = dated timestamps (§4). Task 5 = docs + 0.1.5.

**Tech Stack:** Existing crate, closed dep list — no date/time crate; reuse the existing civil-date math.

## Global Constraints

- Spec above is the contract; all prior invariants bind (read-only, token hygiene, 8-call-plus-bounded-extras budget, no tokio). House gate per commit: clippy pedantic `-D warnings`; `cargo test --all-features` (305 today); `cargo fmt --all` + discard CRLF-only rewrites; dense doc comments; sanctioned-mutator discipline for cursor/tab/view. Branch `focus-and-fixes`. Trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

---

### Task 1: `dm_allow` config + always-subscribed DMs

**Files:**
- Modify: `src/config.rs` (`dm_allow: Vec<String>`, default empty, house key contract — array of non-empty strings, no format restriction beyond that), `src/model.rs` (`resolve_channels` signature gains `dm_allow: &[String]`; allow-listed DMs — exact case-insensitive match on the IM/MPIM counterpart's resolved name — included unconditionally, excluded from the `dm_limit` cut of the *remaining* pool; `dms = false` still suppresses all DMs including allow-listed), call sites in `src/app.rs` and `src/cli.rs` updated.

**Interfaces:**
- `config::PluginConfig::dm_allow(&self) -> &[String]`.
- `model::resolve_channels(config_channels, dms, dm_limit, dm_allow, all) -> Result<Vec<Conversation>, String>`.

- [ ] TDD: config key contract test; `resolve_channels` — allow-listed DM included even when it would rank outside `dm_limit` by recency; allow-list doesn't double-count toward the cap (cap applies only to the non-allow-listed remainder); `dms=false` suppresses allow-listed DMs too; case-insensitive exact match (not substring — a name that merely *contains* an allow-list entry must NOT match). Full gate. Commit `feat: dm_allow always-subscribes named DMs`.

---

### Task 2: Out-of-cap DM activity detection in polling mode

**Files:**
- Modify: `src/app.rs` (`poll_tick_at`: after the existing conversation+thread batch, a separate longer-period cursor — `next_dm_scan: Option<Instant>`, 5-minute interval, injected-clock testable — triggers at most once per tick: re-`list_conversations`, diff `updated` against last-seen per DM outside the currently-resolved set, for each DM whose `updated` moved, issue exactly ONE `history(oldest=last-known-updated)` call (cap: at most 1 extra call this tick regardless of how many DMs changed — pick the single most-recently-updated one; the rest wait for the next 5-minute scan), upsert results normally so Focus/Mentions see them).
- Test: pure scheduling (5-min gate fires/doesn't; picks the single most-recent DM when multiple changed; zero extra calls when nothing changed); the `list_conversations` diff logic as a pure fn over two `Vec<Conversation>` snapshots.

- [ ] TDD; full gate; commit `feat: detect new activity in out-of-cap DMs during polling`.

---

### Task 3: Focus mode

**Files:**
- Modify: `src/config.rs` (`focus_keywords: Vec<String>`, default empty, distinct from the existing `keywords`), `src/app.rs` (`FeedView` gains `Focus`; `toggle_focus()` (Feed-tab-only, mutually exclusive with Threads — toggling Focus from Threads switches to Focus, not both); a `session_watermark: u64` field set from `arrival_seq` at the end of `build` (backfilled messages get `arrival_seq` below it, live ones at/above); `focus_rows_with_ids()` projection: messages with `arrival_seq >= session_watermark` AND (conversation is in `dm_allow` OR text matches any `focus_keywords`, case-insensitive substring, same rule as existing mention-keyword matching) — ascending, same row rendering as timeline), `src/ui.rs` (Focus mode uses timeline row rendering; tab bar marker `1 Focus`; footer `f` hint), `src/lib.rs` (`f` key → `toggle_focus()`, Feed tab only).
- Test: pure qualification (watermark boundary, allow-list-only match, keyword-only match, neither, both — OR semantics); toggle mutual exclusion with Threads; render: Focus view snapshot, footer hint, empty-Focus state (no qualifying messages yet).

- [ ] TDD; full gate; commit `feat: focus mode filtering live messages by allow-list or keyword`.

---

### Task 4: Dated timestamps

**Files:**
- Modify: `src/app.rs` (rename `ts_to_hhmm` → `format_ts(ts: &str, now: SystemTime) -> String`; same-UTC-day as `now` → `HH:MM`; earlier day → `Mon DD HH:MM` using the existing civil-date conversion already present in the codebase for epoch math — locate and reuse it, do not reimplement; month abbreviations as a small const array, no new dependency; call sites pass `SystemTime::now()`, tests inject fixed instants).
- Test: same-day formatting unchanged; prior-day formatting incl. a UTC midnight-boundary case (23:59 yesterday UTC vs 00:01 today UTC using a fixed `now`); month-name correctness for at least two months.

- [ ] TDD; full gate; commit `feat: show the date on timestamps from prior days`.

---

### Task 5: Docs + 0.1.5 release prep

- Modify: `README.md` (config table: `dm_allow`, `focus_keywords`; Rate limits paragraph note that DM cap never blocks new arrivals in either mode; new Focus-mode section: what qualifies, `f` key, session-only scope; Controls table `f` row; timestamp format note), `specs/config.md` (`dm_allow`, `focus_keywords` key contracts), `specs/pane.md` (Focus view contract rows; dated-timestamp rule), `specs/slack-host.md` (out-of-cap poll detection semantics, the 1-extra-call bound), `CHANGELOG.md` ([0.1.5], user-visible voice — lead with the dm_limit fix since it's a bug fix), versions → 0.1.5 (3 files + lock).
- [ ] Full gate; commit `docs: focus mode, dm_allow, dated timestamps; release 0.1.5 prep`.

---

## Self-Review Notes
- Spec §1→T2, §2→T1, §3→T3, §4→T4, docs→T5. Non-goals held (no CLI change, no Threads-Focus hybrid, no new date crate).
- Type consistency: `resolve_channels`'s new signature (T1) is the one every later task's call sites use; `session_watermark`/`FeedView::Focus` (T3) independent of T2/T4; `format_ts` (T4) independent, but T3's render tests should use the renamed fn once T4 lands — sequence T1→T2→T3→T4→T5 as ordered above, or note if implementers find T3 before T4 easier (both orders are fine since Focus doesn't depend on timestamp format; flagged as flexible).
