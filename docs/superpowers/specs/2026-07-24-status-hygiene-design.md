# Status hygiene — design

Date: 2026-07-24
Status: approved (diagnosis session with Dan, 2026-07-24)

## Goal

Three fixes born from one overnight incident: a transient empty-body HTTP response left
`dm scan: Other("invalid JSON: EOF …")` on screen for hours, indistinguishable from a live
failure, with no HTTP status to name the culprit — and a manual `r` refresh gave no sign of
finishing. Statuses are write-once and ageless; this change makes errors self-dating and
self-describing, and gives the refresh sweep visible progress and an end.

## Decisions

1. **Invalid-JSON errors carry the HTTP status** when the curl write-out trailer parsed:
   `invalid JSON (HTTP 302): EOF …`. No trailer (curl < 7.83) keeps today's wording — there
   is no status to report.
2. **Recurring error statuses get a UTC `HH:MM` prefix** — `03:12 dm scan: …`,
   `03:12 slack rate limit — retrying in 30s` — applied in `poll_error_status`, the one pure
   choke point every poll/scan/thread error routes through. Interactive-keypress errors
   (permalink, replies, thread refresh) stay unprefixed: the user is present when they fire.
   The pane already renders times as UTC `HH:MM` (`format_ts`); same convention, no new
   dependency.
3. **The catch-up sweep narrates progress and completion.** Each `catchup_tick` batch that
   finishes without writing an error status updates the line to
   `refreshing <remaining> conversations`, and the batch that drains the sweep writes
   `HH:MM refresh complete`. A batch that *did* surface an error (or rate limit) leaves that
   error on screen — the countdown never overwrites fresher bad news (guard: compare status
   before/after the batch). Applies to the same sweep when armed by a socket reconnect: the
   post-reconnect catch-up becomes visible the same way, which replaces the stale
   `socket unavailable — polling` line it previously left behind.

## Design

- `src/rest.rs` `parse_response`: the JSON-parse `map_err` branches on the already-split
  trailer to append `(HTTP <code>)`.
- `src/app.rs`: new private pure `hhmm_utc(now_secs: u64) -> String` (the `day_secs` math
  extracted from `format_ts`); `poll_error_status` becomes a thin wrapper over
  `poll_error_status_at(now_secs, …)` so call sites stay unchanged and tests inject the
  clock. `catchup_tick_at` gets the status-guarded countdown/completion described above
  (completion stamped via `hhmm_utc(users_cache::now_secs())`; tests assert the `NN:NN `
  shape rather than pinning the wall clock).
- No signature changes visible outside `app.rs`/`rest.rs`; no new dependencies.

## Testing

Pure-function tests throughout: trailer/no-trailer invalid-JSON wording; `hhmm_utc` edges
(midnight, `% 86_400` rollover); `poll_error_status_at` exact prefixed strings for both arms;
catchup countdown/completion/error-preservation via the existing `precancelled_rest` fixture
style. Live smoke: none needed — all behavior is renderable locally.

## Out of scope

- Timestamping interactive-keypress errors or the `socket unavailable` transition status.
- Auto-clearing statuses on a timer, or a general status-history mechanism.
- Following proxy redirects (`curl -L`) or retrying empty-body responses.
