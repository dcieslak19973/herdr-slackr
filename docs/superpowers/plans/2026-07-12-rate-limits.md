# Rate-Limit Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop slackr from hammering Slack: DM cap, incremental staggered polling with real Retry-After cooldowns, a shared users cache, and resilient startup backfill.

**Architecture:** Five targeted changes per `docs/superpowers/specs/2026-07-12-rate-limit-hardening-design.md` — no new modules; changes land in `config.rs` (dm_limit), `app.rs` (poll scheduling, cooldown, backfill retry, per-conv ts tracking), `rest.rs` (write-out trailer, Retry-After), a small `users_cache.rs` (or a section of rest.rs — implementer's call, one clear owner), and `cli.rs` (cache reuse).

**Tech Stack:** Existing crate, closed dep list, no additions.

## Global Constraints

- Spec: `2026-07-12-rate-limit-hardening-design.md` — the contract; the base specs' invariants (read-only, token hygiene, no tokio) bind.
- House gate per commit: clippy pedantic `-D warnings`; `cargo test --all-features` (178 green today); `cargo fmt --all` + discard CRLF-only rewrites. Dense doc comments. Branch `rate-limits`. Trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

---

### Task 1: dm_limit + Retry-After trailer + users cache

**Files:**
- Modify: `src/config.rs` (`dm_limit`: u32, default 20, valid 0..=200, house key contract + tests), `src/rest.rs` (write-out trailer + parse; `users_cached` entry point), `src/model.rs` only if `Conversation` needs an `updated: Option<u64>` field (it does — thread it through `parse_conversations` and fixtures), `src/app.rs`+`src/cli.rs` (`resolve_channels` gains the cap; both call `users_cached`)
- Create: `src/users_cache.rs` (load/store/TTL; state-dir resolution env-else-`~/.local/state/herdr/plugins/dcieslak19973.slackr`)
- Test: unit tests per spec §Testing; CLI cache-hit integration test with a pre-seeded `users.json` in a fixture state dir (env-injected).

**Interfaces:**
- `config::PluginConfig::dm_limit() -> u32`.
- `rest::RestError::RateLimited(u64)` now carries the server's Retry-After when present (parse fn: split the final `\n<code> <retry-after>` trailer; absent trailer → legacy path; 429 → RateLimited(header or 30)). Pure, fixture-tested for the four cases in the spec.
- `users_cache::{load(state_dir, now) -> Option<Vec<(String,String)>>, store(state_dir, &users, now), state_dir(env_fn, home_fn) -> Option<PathBuf>}` — TTL 24h decided in `load` via injected now; 0600 on Unix; best-effort store.
- `resolve_channels(config_channels, dms, dm_limit, all) -> Result<Vec<Conversation>, String>` — cap by `updated` desc (absent → list-order fallback + `logln!`). Update BOTH copies (app.rs and cli.rs) — or better, deduplicate now: move it to `model.rs` as `pub(crate)` and delete the copies (the filed fast-follow; in scope here since the signature changes anyway).

- [ ] **Step 1:** Failing tests (config key contract; trailer parse ×4; users_cache TTL ×3 + state_dir fallback; resolve_channels cap ×3 incl. updated-absent). **Step 2:** RED. **Step 3:** implement (dedupe resolve_channels into model.rs while at it). **Step 4:** gate. **Step 5:** commit `feat: dm_limit, real Retry-After, shared users cache`.

---

### Task 2: Poll scheduling + cooldown + backfill retry

**Files:**
- Modify: `src/app.rs` (per-conv newest-ts map — derive from the existing message store or track on upsert; `poll_tick` gains `oldest`, `POLL_BATCH = 8` round-robin via a pure `fn next_batch(cursor, n, batch) -> (Range indices, new cursor)`; cooldown deadline field checked at tick entry, set from `RateLimited(secs)`; `build` backfill: one sleep-and-retry on RateLimited then skip-remaining-with-notice), `src/rest.rs` (`history` gains `oldest: Option<&str>` param — update the CLI call sites with `None`), `src/lib.rs` (event loop passes a now/elapsed the cooldown needs, or App owns an Instant — keep it App-internal with injected now for tests)
- Test: pure tests per spec (`next_batch` wrap-around; cooldown gating with injected clock; oldest threading — history arg asserted via a parse-level test or the curl-args pattern; backfill retry decision as a pure fn).

**Interfaces:**
- Consumes Task 1's `RateLimited(real_secs)`.
- `rest::history(rest, conv, limit, oldest: Option<&str>)` — `oldest` percent-encoded query param.

- [ ] **Step 1:** failing tests. **Step 2:** RED. **Step 3:** implement. **Step 4:** gate (the existing poll_tick tests update for the new scheduling — assert meaning, e.g. all convs eventually visited across ticks). **Step 5:** commit `feat: incremental staggered polling with rate-limit cooldowns`.

---

### Task 3: Docs, version, release prep

- Modify: `README.md` (config table + a short "Rate limits" paragraph: what the defaults mean, dm_limit semantics incl. socket-events-outside-the-cap note), `specs/config.md` (dm_limit), `specs/slack-host.md` (+ polling/cooldown/Retry-After semantics), `specs/agent-cli.md` (users cache note), `CHANGELOG.md` ([0.1.2]), versions → `0.1.2` (Cargo.toml + herdr-plugin.toml + lock).
- [ ] Docs accurate to code (read the landed implementations); full gate; commit `docs: rate-limit semantics; release 0.1.2 prep`.

---

## Self-Review Notes
- Spec coverage: §1→T1, §2→T2, §3→T1, §4→T1, §5→T2, docs→T3. Non-goals held (no token bucket, no persistence beyond users cache).
- Type consistency: `RateLimited(u64)` unchanged in shape; `history` signature change named in both tasks; `resolve_channels` dedup declared in T1 and consumed in T2's poll set.
