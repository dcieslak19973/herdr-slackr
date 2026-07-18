//! Slack Web API access via `curl`: read-only REST calls for conversations, messages, users,
//! and permalinks, plus the one REST call Socket Mode needs (`apps.connections.open`). See
//! `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md`.
//!
//! Every call runs `curl --silent --show-error --config - <url>` with the bearer token carried
//! on stdin as `header = "Authorization: Bearer <token>"` — never in argv, so it never appears
//! in `ps`/`/proc`'s visible arguments (same shape as `herdr-reviewr`'s Bitbucket backend).
//!
//! **No `--fail`.** Slack's Web API answers HTTP 200 even on failure, wrapping the error as
//! `{"ok": false, "error": "..."}` in the body — `--fail` only maps *HTTP status* failures to a
//! curl exit code, so it would never see a Slack-level error and buys nothing here. Instead
//! every response is parsed regardless of transport outcome, and `ok` is checked on every call.
//! A `ratelimited` error is Slack's signal for HTTP 429; Slack does not echo the triggering
//! `Retry-After` value into that JSON body, so every call also appends
//! `--write-out '\n%{http_code} %header{retry-after}'` (curl ≥ 7.83) — [`parse_response`] splits
//! that trailer off the body and [`RestError::RateLimited`] carries the server's real
//! `Retry-After` seconds when the trailer names one, defaulting to 30 when it's missing (no
//! header sent, or — on curl < 7.83 — no trailer at all, in which case parsing degrades exactly
//! to the pre-trailer behavior: the whole output is the body).
#![allow(clippy::result_large_err)]

use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::model::{ConvKind, Conversation, Message};

const BASE: &str = "https://slack.com/api/";

/// One REST session: the user token (`xoxp-...`) used for every Web API call in this module
/// except [`connections_open`], and the cancellation flag shared with the rest of the fetch.
pub struct Rest<'a> {
    pub user_token: &'a str,
    pub cancelled: &'a AtomicBool,
}

impl std::fmt::Debug for Rest<'_> {
    /// Redacts `user_token`: never echo the bearer token, matching `tokens.rs`'s never-echo
    /// discipline. Only the sentinel is rendered, regardless of the token's actual value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rest")
            .field("user_token", &"<redacted>")
            .field("cancelled", &self.cancelled)
            .finish()
    }
}

/// A classified REST failure.
#[derive(Debug, PartialEq, Eq)]
pub enum RestError {
    /// `curl` is not on `PATH`.
    NoCurl,
    /// Slack answered `{"ok": false, "error": "..."}`; the `error` field verbatim
    /// (`invalid_auth`, `channel_not_found`, …).
    SlackError(String),
    /// Slack answered `{"ok": false, "error": "ratelimited"}`, or the write-out trailer's HTTP
    /// code was 429; the suggested backoff in seconds — the server's real `Retry-After` when
    /// the trailer carries one, else the 30s default (see the module doc).
    RateLimited(u64),
    /// Any other transport, I/O, or JSON-shape failure.
    Other(String),
}

impl Rest<'_> {
    /// GET `method` with `params` (percent-encoded query values), parse the JSON body, and map
    /// a non-`ok` response, a `ratelimited` error, or a transport failure to [`RestError`].
    pub fn get(&self, method: &str, params: &[(&str, &str)]) -> Result<Value, RestError> {
        let query = build_query(params);
        let url = if query.is_empty() {
            format!("{BASE}{method}")
        } else {
            format!("{BASE}{method}?{query}")
        };
        let config = curl_config(self.user_token);
        let args = get_args(&url);
        let out = crate::proc::run_tool("curl", &args, Some(&config), self.cancelled)
            .map_err(classify)?;
        parse_response(&out)
    }
}

/// The socket worker's one REST need, authenticated with the APP token (`xapp-...`) instead of
/// the user token: open a Socket Mode connection and return its `wss://` URL.
pub fn connections_open(app_token: &str, cancelled: &AtomicBool) -> Result<String, RestError> {
    let url = format!("{BASE}apps.connections.open");
    let config = curl_config(app_token);
    let args = post_args(&url);
    let out = crate::proc::run_tool("curl", &args, Some(&config), cancelled).map_err(classify)?;
    let v = parse_response(&out)?;
    v["url"].as_str().map(str::to_string).ok_or_else(|| RestError::Other("missing url".to_string()))
}

/// All subscribed conversations (channels, groups, IMs, MPIMs), paginated to completion. Used
/// by `App::build`'s one-time channel-name resolution, which genuinely needs every kind.
pub fn list_conversations(rest: &Rest) -> Result<Vec<Conversation>, RestError> {
    list_conversations_of("public_channel,private_channel,im,mpim", rest)
}

/// Only the workspace's IMs and MPIMs, paginated to completion. The recurring out-of-cap DM
/// activity scan (`App::maybe_scan_out_of_cap_dms`) calls this instead of
/// [`list_conversations`]: the scan only ever selects `Im`/`Mpim` candidates, and the full
/// list pages through *every public channel in the workspace* — on a large workspace that is
/// dozens of Tier-2 requests every scan interval, spent on rows the scan then filters out.
pub fn list_dm_conversations(rest: &Rest) -> Result<Vec<Conversation>, RestError> {
    list_conversations_of("im,mpim", rest)
}

/// The shared `conversations.list` pagination loop behind [`list_conversations`] /
/// [`list_dm_conversations`], parameterized by the `types` filter.
fn list_conversations_of(types: &str, rest: &Rest) -> Result<Vec<Conversation>, RestError> {
    let mut cursor = String::new();
    let mut out = Vec::new();
    loop {
        let params = conversation_list_params(types, &cursor);
        let v = rest.get("conversations.list", &params)?;
        out.extend(parse_conversations(&v));
        match next_cursor(&v) {
            Some(c) => cursor = c,
            None => break,
        }
    }
    Ok(out)
}

/// Pure param-list construction for [`list_conversations_of`], split out (like
/// [`history_params`]) so the `types` threading is unit-tested without a real REST call.
fn conversation_list_params<'a>(types: &'a str, cursor: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![("types", types), ("limit", "200"), ("cursor", cursor)]
}

/// How many pages an incremental [`history`] fetch may follow at most — a defensive bound so a
/// firehose channel (or a bad cursor loop) can't turn one poll slot into an unbounded request
/// storm. At `limit` 50 this covers a 500-message burst per conversation per poll; a burst
/// beyond that loses its *oldest* remainder (pages run newest → older), which is logged rather
/// than silent.
const HISTORY_PAGE_CAP: usize = 10;

/// The most recent `limit` messages in `conv`, or — when `oldest` names a `ts` — every message
/// newer than it (Slack's `oldest` is exclusive by default, which is exactly what incremental
/// polling wants: the tracked newest-seen `ts` is passed back in, so the common tick's response
/// body is empty rather than re-shipping the same 50 messages every time). An incremental fetch
/// follows `response_metadata.next_cursor` up to [`HISTORY_PAGE_CAP`] pages (Slack answers with
/// the newest `limit` first and pages older): without pagination, a burst of more than `limit`
/// messages between polls would silently lose its middle — the caller's newest-seen watermark
/// advances past the gap and no later fetch ever asks for it again. `None` fetches the plain
/// last-`limit` page, one page by design, used for the initial backfill and the CLI (both want
/// the freshest window regardless of anything previously seen).
///
/// A mid-pagination failure (including `RateLimited`) discards the pages already fetched and
/// returns the error: nothing was folded in, so the caller's watermark is untouched and the
/// whole span is re-fetched cleanly after the cooldown.
///
/// Note the page cap is deliberately *larger* than `App`'s per-conversation retention
/// (`MAX_PER_CONV`, currently 300 vs the 500 messages ten 50-message pages can carry): every
/// fetched message runs through the mention scan before the prune considers it, and the prune
/// exempts unread mentions — so the pages beyond retention still surface a mention buried deep
/// in a burst even though their ordinary messages are pruned right away.
pub fn history(
    rest: &Rest,
    conv: &str,
    limit: u32,
    oldest: Option<&str>,
) -> Result<Vec<Message>, RestError> {
    history_counted(rest, conv, limit, oldest).map(|(msgs, _)| msgs)
}

/// As [`history`], additionally reporting how many requests (pages) the fetch actually issued —
/// what `App::poll_conversations`'s request-budget accounting needs, since a paginated catch-up
/// fetch can cost up to [`HISTORY_PAGE_CAP`] requests while a caught-up conversation costs one.
pub fn history_counted(
    rest: &Rest,
    conv: &str,
    limit: u32,
    oldest: Option<&str>,
) -> Result<(Vec<Message>, usize), RestError> {
    let limit = limit.to_string();
    let mut out = Vec::new();
    let mut cursor = String::new();
    let mut pages = 0;
    loop {
        let params = history_params(conv, &limit, oldest, &cursor);
        let v = rest.get("conversations.history", &params)?;
        out.extend(parse_messages(&v, conv));
        pages += 1;
        let more = next_cursor(&v);
        let capped = more.is_some();
        let Some(c) = next_history_page(oldest.is_some(), more, pages) else {
            if oldest.is_some() && capped && pages >= HISTORY_PAGE_CAP {
                crate::logln!(
                    "history: page cap ({HISTORY_PAGE_CAP}) reached for {conv}; \
                     the burst's oldest messages were skipped"
                );
            }
            break;
        };
        cursor = c;
    }
    Ok((out, pages))
}

/// Pure page-continuation decision for [`history`]: follow `cursor` for another page only when
/// the fetch is incremental (`oldest` was given — the plain freshest-window fetch is one page
/// by design), Slack reported a next page at all, and fewer than [`HISTORY_PAGE_CAP`] pages
/// have been fetched. Split out so the gate is unit-tested without a real REST call.
fn next_history_page(
    incremental: bool,
    cursor: Option<String>,
    pages_fetched: usize,
) -> Option<String> {
    if !incremental || pages_fetched >= HISTORY_PAGE_CAP {
        return None;
    }
    cursor
}

/// Pure param-list construction for [`history`], split out so `oldest`'s presence/absence is
/// unit-tested without a real REST call. `channel`/`limit` are always sent; `oldest` is appended
/// only when given — omitting the key entirely rather than sending an empty value, matching how
/// `list_conversations`/`users` treat their own optional-cursor case — and `cursor` only when
/// non-empty (the first page has none).
fn history_params<'a>(
    conv: &'a str,
    limit: &'a str,
    oldest: Option<&'a str>,
    cursor: &'a str,
) -> Vec<(&'a str, &'a str)> {
    let mut params = vec![("channel", conv), ("limit", limit)];
    if let Some(oldest) = oldest {
        params.push(("oldest", oldest));
    }
    if !cursor.is_empty() {
        params.push(("cursor", cursor));
    }
    params
}

/// All replies in the thread rooted at `thread_ts` within `conv`, or — when `oldest` names a
/// `ts` — only those newer than it (same exclusive-`oldest` semantics as [`history`], used by
/// the polling fallback's bounded thread refresh: the thread's newest-known reply ts is passed
/// back in so a routine tick only asks Slack for replies it hasn't already stored). `None` fetches
/// every reply, used by the interactive Enter-to-expand path (which wants the whole thread).
pub fn replies(
    rest: &Rest,
    conv: &str,
    thread_ts: &str,
    oldest: Option<&str>,
) -> Result<Vec<Message>, RestError> {
    let params = replies_params(conv, thread_ts, oldest);
    let v = rest.get("conversations.replies", &params)?;
    Ok(parse_messages(&v, conv))
}

/// Pure param-list construction for [`replies`], split out exactly like [`history_params`] so
/// `oldest`'s presence/absence is unit-tested without a real REST call.
fn replies_params<'a>(
    conv: &'a str,
    thread_ts: &'a str,
    oldest: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut params = vec![("channel", conv), ("ts", thread_ts)];
    if let Some(oldest) = oldest {
        params.push(("oldest", oldest));
    }
    params
}

/// Every workspace member as `(id, display name)`, paginated to completion. The name is
/// `profile.display_name` when set, else `profile.real_name`, else the top-level `real_name`,
/// else the id itself.
pub fn users(rest: &Rest) -> Result<Vec<(String, String)>, RestError> {
    let mut cursor = String::new();
    let mut out = Vec::new();
    loop {
        let v = rest.get("users.list", &[("limit", "200"), ("cursor", cursor.as_str())])?;
        out.extend(parse_users(&v));
        match next_cursor(&v) {
            Some(c) => cursor = c,
            None => break,
        }
    }
    Ok(out)
}

/// A permalink URL for one message.
pub fn permalink(rest: &Rest, conv: &str, ts: &str) -> Result<String, RestError> {
    let v = rest.get("chat.getPermalink", &[("channel", conv), ("message_ts", ts)])?;
    v["permalink"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| RestError::Other("missing permalink".to_string()))
}

/// The authenticated user's own id, for self-mention detection.
pub fn auth_self(rest: &Rest) -> Result<String, RestError> {
    let v = rest.get("auth.test", &[])?;
    v["user_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| RestError::Other("missing user_id".to_string()))
}

// ---- Transport ------------------------------------------------------------------------------

/// The stdin curl config carrying the bearer token — never argv, so it never appears in
/// `ps`/`/proc`'s visible arguments.
fn curl_config(token: &str) -> String {
    format!("header = \"Authorization: Bearer {token}\"\n")
}

/// The `--write-out` format appended to every call: a leading `\n` so [`split_trailer`] can
/// find it as the output's last line, then the HTTP status and the `Retry-After` header value
/// (empty when the header is absent). Requires curl ≥ 7.83 for `%header{}`; see the module doc
/// for the older-curl degradation.
const WRITE_OUT: &str = "\n%{http_code} %header{retry-after}";

/// The GET args: no `--fail` (see the module doc), config from stdin (`-`), plus the
/// Retry-After write-out trailer.
fn get_args(url: &str) -> Vec<&str> {
    vec!["--silent", "--show-error", "--write-out", WRITE_OUT, "--config", "-", url]
}

/// As [`get_args`], but forcing a POST — the only caller is [`connections_open`].
fn post_args(url: &str) -> Vec<&str> {
    vec![
        "--silent",
        "--show-error",
        "--write-out",
        WRITE_OUT,
        "--request",
        "POST",
        "--config",
        "-",
        url,
    ]
}

/// `NotFound` → [`RestError::NoCurl`]; a cancelled/IO failure or a transport-level `curl`
/// failure (no HTTP response at all — refused connection, DNS, TLS, timeout) → `Other`. A
/// non-2xx HTTP status is not in this match at all: without `--fail`, curl treats it as success
/// and this module reads Slack's `ok`/`error` fields from the body instead (module doc).
fn classify(f: crate::proc::RunFail) -> RestError {
    match f {
        crate::proc::RunFail::NotFound => RestError::NoCurl,
        crate::proc::RunFail::Cancelled => RestError::Other("request cancelled".to_string()),
        crate::proc::RunFail::Io(message) => RestError::Other(message),
        crate::proc::RunFail::Failed { stderr } => RestError::Other(stderr.trim().to_string()),
    }
}

/// The write-out trailer's HTTP status and optional `Retry-After` seconds, once split off the
/// response body by [`split_trailer`].
type Trailer = (u16, Option<u64>);

/// Split `out`'s trailing `\n<http_code> <retry-after>` line (appended by [`WRITE_OUT`]) off
/// the response body. Returns `(body, None)` when the last line isn't a valid trailer — no
/// trailing newline at all, or a non-numeric status — which is exactly what curl < 7.83
/// produces (module doc): the whole output is the body, same as before this trailer existed.
/// A present but empty `Retry-After` value (header not sent) yields `Some((code, None))`.
fn split_trailer(out: &str) -> (&str, Option<Trailer>) {
    let Some(idx) = out.rfind('\n') else { return (out, None) };
    let tail = out[idx + 1..].trim_end();
    let mut parts = tail.splitn(2, ' ');
    let Some(Ok(code)) = parts.next().map(str::parse::<u16>) else { return (out, None) };
    let retry_after =
        parts.next().map(str::trim).filter(|s| !s.is_empty()).and_then(|s| s.parse().ok());
    (&out[..idx], Some((code, retry_after)))
}

/// Split the write-out trailer off `out`, parse the remaining body as JSON, and apply
/// [`check_ok`]. Split from `check_ok` so the ok/error decision is pure and fixture-testable
/// without a JSON parse in the way.
///
/// The trailer is consulted *before* the JSON parse: a corporate-proxy/WAF 429 typically comes
/// back with an HTML (non-JSON) body, and the trailer's HTTP status is authoritative regardless
/// of what the body contains. Deferring to the JSON parse first would surface that as
/// `Other("invalid JSON...")` instead of the `RateLimited` the caller needs to back off on.
fn parse_response(out: &str) -> Result<Value, RestError> {
    let (body, trailer) = split_trailer(out);
    if trailer.is_some_and(|(code, _)| code == 429) {
        let secs = trailer.and_then(|(_, retry_after)| retry_after).unwrap_or(30);
        return Err(RestError::RateLimited(secs));
    }
    let v: Value =
        serde_json::from_str(body).map_err(|e| RestError::Other(format!("invalid JSON: {e}")))?;
    check_ok(v, trailer)
}

/// Slack's uniform envelope: `ok: true` passes `v` through; `ok: false` reads `error`. A
/// `ratelimited` error, or a trailer reporting HTTP 429, maps to [`RestError::RateLimited`]
/// carrying the trailer's `Retry-After` seconds when present, else the 30s default (module doc).
/// Anything else maps to [`RestError::SlackError`] verbatim.
fn check_ok(v: Value, trailer: Option<Trailer>) -> Result<Value, RestError> {
    if v["ok"].as_bool() == Some(true) {
        return Ok(v);
    }
    let error = v["error"].as_str().unwrap_or("unknown_error");
    let is_429 = trailer.is_some_and(|(code, _)| code == 429);
    if error == "ratelimited" || is_429 {
        let secs = trailer.and_then(|(_, retry_after)| retry_after).unwrap_or(30);
        return Err(RestError::RateLimited(secs));
    }
    Err(RestError::SlackError(error.to_string()))
}

/// Percent-encode for a URL query value: unreserved chars pass, all else `%XX`. Copied from
/// `herdr-reviewr`'s `src/forge/mod.rs::enc`.
fn enc(s: &str) -> String {
    s.bytes()
        .flat_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![b as char]
            }
            _ => format!("%{b:02X}").chars().collect(),
        })
        .collect()
}

/// Build a `key=value&...` query string, each key and value percent-encoded independently.
fn build_query(params: &[(&str, &str)]) -> String {
    params.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

// ---- Pure parsing (unit-tested on fixtures) --------------------------------------------------

/// `response_metadata.next_cursor`, treating an absent or empty cursor as "no more pages" —
/// Slack's own convention for the last page.
fn next_cursor(v: &Value) -> Option<String> {
    v["response_metadata"]["next_cursor"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// `conversations.list`'s `channels[]` to [`Conversation`]s. Kind is read off Slack's four
/// boolean flags, checked `im` → `mpim` → `group` → default `channel` (a channel object may not
/// set `is_channel` explicitly on every API version, so it is the fallback rather than a
/// required flag). An IM's `name` is its counterpart `user` id — Slack has no channel-style name
/// for a DM — resolved to a display name later by pairing with [`users`]'s output. A channel's
/// `name` prefers `name_normalized` (Slack's canonicalized form) and falls back to `name`.
/// `updated` (millisecond epoch of last activity) is carried through as `None` when Slack's
/// payload omits it — some workspaces don't send it — rather than defaulted to any value.
fn parse_conversations(v: &Value) -> Vec<Conversation> {
    v["channels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let id = c["id"].as_str()?.to_string();
            let kind = if c["is_im"].as_bool() == Some(true) {
                ConvKind::Im
            } else if c["is_mpim"].as_bool() == Some(true) {
                ConvKind::Mpim
            } else if c["is_group"].as_bool() == Some(true) {
                ConvKind::Group
            } else {
                ConvKind::Channel
            };
            let name = if kind == ConvKind::Im {
                c["user"].as_str().unwrap_or_default().to_string()
            } else {
                c["name_normalized"]
                    .as_str()
                    .or_else(|| c["name"].as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let updated = c["updated"].as_u64();
            Some(Conversation { id, name, kind, updated })
        })
        .collect()
}

/// `conversations.history`/`conversations.replies`'s `messages[]` to [`Message`]s, in the order
/// Slack returned them (this module never re-sorts). `thread_ts` becomes `None` when absent *or*
/// equal to the message's own `ts` — Slack sets a thread root's `thread_ts` to its own `ts`, and
/// per `Message`'s contract that case is a top-level message, not a reply to itself. `reply_count`
/// is read straight off the object (only a thread root carries it; a reply or a threadless
/// message simply has no such field, yielding `None`).
fn parse_messages(v: &Value, conv: &str) -> Vec<Message> {
    v["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let ts = m["ts"].as_str().unwrap_or_default().to_string();
            let thread_ts = m["thread_ts"].as_str().filter(|t| *t != ts).map(str::to_string);
            let reply_count = m["reply_count"].as_u64().and_then(|n| u32::try_from(n).ok());
            Message {
                conv: conv.to_string(),
                ts,
                thread_ts,
                author: m["user"].as_str().unwrap_or_default().to_string(),
                text: m["text"].as_str().unwrap_or_default().to_string(),
                edited: !m["edited"].is_null(),
                reply_count,
            }
        })
        .collect()
}

/// `users.list`'s `members[]` to `(id, name)` pairs; name resolution order in the function doc.
fn parse_users(v: &Value) -> Vec<(String, String)> {
    v["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| {
            let id = u["id"].as_str()?.to_string();
            let display = u["profile"]["display_name"].as_str().filter(|s| !s.is_empty());
            let profile_real = u["profile"]["real_name"].as_str().filter(|s| !s.is_empty());
            let top_real = u["real_name"].as_str().filter(|s| !s.is_empty());
            let name = display.or(profile_real).or(top_real).unwrap_or(&id).to_string();
            Some((id, name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- curl-arg construction: token stays out of argv -------------------------------------

    #[test]
    fn curl_config_carries_the_bearer_token_as_a_header_line() {
        let config = curl_config("xoxp-secret");
        assert_eq!(config, "header = \"Authorization: Bearer xoxp-secret\"\n");
    }

    #[test]
    fn get_args_never_puts_the_token_or_bearer_header_in_argv() {
        let args = get_args("https://slack.com/api/auth.test");
        assert_eq!(
            args,
            vec![
                "--silent",
                "--show-error",
                "--write-out",
                WRITE_OUT,
                "--config",
                "-",
                "https://slack.com/api/auth.test"
            ]
        );
        assert!(!args.iter().any(|a| a.contains("Bearer") || a.contains("Authorization")));
        assert!(!args.contains(&"--fail")); // module doc: --fail is deliberately absent
    }

    #[test]
    fn get_args_appends_the_retry_after_write_out_trailer() {
        let args = get_args("https://slack.com/api/auth.test");
        assert!(args.contains(&"--write-out"));
        assert!(args.contains(&"\n%{http_code} %header{retry-after}"));
    }

    #[test]
    fn post_args_forces_post_and_still_keeps_the_token_off_argv() {
        let args = post_args("https://slack.com/api/apps.connections.open");
        assert!(args.contains(&"--request"));
        assert!(args.contains(&"POST"));
        assert!(!args.iter().any(|a| a.contains("Bearer")));
        assert!(args.contains(&"--write-out")); // same trailer parsing as get_args
    }

    #[test]
    fn debug_format_redacts_the_bearer_token() {
        let cancelled = AtomicBool::new(false);
        let rest = Rest { user_token: "xoxp-super-secret-token", cancelled: &cancelled };
        let debug = format!("{rest:?}");
        assert!(!debug.contains("xoxp-super-secret-token"));
        assert!(debug.contains("redacted"));
    }

    // ---- enc / query building ----------------------------------------------------------------

    #[test]
    fn enc_passes_unreserved_and_percent_encodes_everything_else() {
        assert_eq!(enc("abcXYZ019-._~"), "abcXYZ019-._~");
        assert_eq!(enc("a b"), "a%20b");
        assert_eq!(enc("public_channel,im"), "public_channel%2Cim");
        assert_eq!(enc("1752300000.000100"), "1752300000.000100");
    }

    #[test]
    fn build_query_encodes_each_key_and_value_independently() {
        assert_eq!(build_query(&[("channel", "C1"), ("limit", "200")]), "channel=C1&limit=200");
        assert_eq!(build_query(&[("cursor", "a b")]), "cursor=a%20b");
        assert_eq!(build_query(&[]), "");
    }

    // ---- conversation_list_params (types threading: the DM scan must not page every public
    // channel in the workspace every five minutes) --------------------------------------------

    #[test]
    fn conversation_list_params_threads_the_requested_types() {
        assert_eq!(
            conversation_list_params("im,mpim", ""),
            vec![("types", "im,mpim"), ("limit", "200"), ("cursor", "")]
        );
    }

    #[test]
    fn conversation_list_params_carries_the_page_cursor() {
        assert_eq!(
            conversation_list_params("public_channel,private_channel,im,mpim", "abc"),
            vec![
                ("types", "public_channel,private_channel,im,mpim"),
                ("limit", "200"),
                ("cursor", "abc")
            ]
        );
    }

    // ---- ok / error envelope -------------------------------------------------------------------

    #[test]
    fn check_ok_passes_through_a_true_ok_response() {
        let v = serde_json::json!({"ok": true, "user_id": "U1"});
        assert_eq!(check_ok(v.clone(), None).unwrap(), v);
    }

    #[test]
    fn check_ok_maps_a_named_slack_error() {
        let v = serde_json::json!({"ok": false, "error": "invalid_auth"});
        assert_eq!(
            check_ok(v, None).unwrap_err(),
            RestError::SlackError("invalid_auth".to_string())
        );
    }

    #[test]
    fn check_ok_maps_ratelimited_with_no_trailer_to_the_thirty_second_default() {
        let v = serde_json::json!({"ok": false, "error": "ratelimited"});
        assert_eq!(check_ok(v, None).unwrap_err(), RestError::RateLimited(30));
    }

    #[test]
    fn check_ok_a_429_trailer_with_no_ratelimited_body_error_still_rate_limits() {
        let v = serde_json::json!({"ok": false, "error": "unknown_error"});
        assert_eq!(check_ok(v, Some((429, Some(12)))).unwrap_err(), RestError::RateLimited(12));
    }

    #[test]
    fn parse_response_surfaces_invalid_json_as_other() {
        let err = parse_response("not json").unwrap_err();
        assert!(matches!(err, RestError::Other(_)));
    }

    // ---- write-out trailer parsing (spec's four cases) ---------------------------------------

    #[test]
    fn trailer_429_with_retry_after_header_carries_the_real_seconds() {
        let out = "{\"ok\":false,\"error\":\"ratelimited\"}\n429 42";
        assert_eq!(parse_response(out).unwrap_err(), RestError::RateLimited(42));
    }

    #[test]
    fn trailer_429_without_retry_after_header_defaults_to_thirty() {
        let out = "{\"ok\":false,\"error\":\"ratelimited\"}\n429 ";
        assert_eq!(parse_response(out).unwrap_err(), RestError::RateLimited(30));
    }

    #[test]
    fn trailer_absent_entirely_degrades_to_legacy_whole_output_as_body() {
        // Older curl (< 7.83): no `%header{}` support, so no trailer line is appended at all —
        // the whole output is the body, exactly as it was before this trailer existed.
        let out = "{\"ok\":true,\"user_id\":\"U1\"}";
        let v = parse_response(out).unwrap();
        assert_eq!(v["user_id"].as_str(), Some("U1"));
    }

    #[test]
    fn trailer_absent_ratelimited_body_still_gets_the_legacy_thirty_second_default() {
        let out = "{\"ok\":false,\"error\":\"ratelimited\"}";
        assert_eq!(parse_response(out).unwrap_err(), RestError::RateLimited(30));
    }

    #[test]
    fn body_says_ratelimited_even_when_the_trailer_status_is_not_429() {
        // Module doc: Slack answers 200 even on failure; the trailer's status is not the only
        // signal a rate limit happened.
        let out = "{\"ok\":false,\"error\":\"ratelimited\"}\n200 7";
        assert_eq!(parse_response(out).unwrap_err(), RestError::RateLimited(7));
    }

    #[test]
    fn split_trailer_handles_the_four_cases() {
        assert_eq!(split_trailer("body\n429 30"), ("body", Some((429, Some(30)))));
        assert_eq!(split_trailer("body\n429 "), ("body", Some((429, None))));
        assert_eq!(split_trailer("body"), ("body", None));
        assert_eq!(split_trailer("body\n200 "), ("body", Some((200, None))));
    }

    #[test]
    fn a_429_trailer_rate_limits_even_when_the_body_is_not_json() {
        // A corporate-proxy/WAF 429 typically returns an HTML body, not Slack's JSON envelope.
        // The trailer must be consulted before the JSON parse is attempted, or this surfaces
        // as `Other("invalid JSON...")` instead of the rate limit the caller needs to react to.
        let out = "<html>Too Many Requests</html>\n429 17";
        assert_eq!(parse_response(out).unwrap_err(), RestError::RateLimited(17));
    }

    #[test]
    fn a_429_trailer_without_retry_after_rate_limits_a_non_json_body_with_the_default() {
        let out = "<html>Too Many Requests</html>\n429 ";
        assert_eq!(parse_response(out).unwrap_err(), RestError::RateLimited(30));
    }

    #[test]
    fn a_non_json_body_with_a_200_trailer_still_surfaces_as_invalid_json() {
        let out = "<html>Too Many Requests</html>\n200 ";
        let err = parse_response(out).unwrap_err();
        assert!(matches!(err, RestError::Other(_)));
    }

    // ---- conversations.list ---------------------------------------------------------------

    #[test]
    fn parse_conversations_reads_all_four_kinds_and_the_im_name_is_the_counterpart_user_id() {
        let v = serde_json::json!({
            "ok": true,
            "channels": [
                {"id": "C1", "name": "general", "name_normalized": "general",
                 "is_channel": true, "is_group": false, "is_im": false, "is_mpim": false},
                {"id": "G1", "name": "priv-team", "name_normalized": "priv-team",
                 "is_channel": false, "is_group": true, "is_im": false, "is_mpim": false},
                {"id": "D1", "user": "U42",
                 "is_channel": false, "is_group": false, "is_im": true, "is_mpim": false},
                {"id": "M1", "name": "mpdm-a-b-1", "name_normalized": "mpdm-a-b-1",
                 "is_channel": false, "is_group": true, "is_im": false, "is_mpim": true}
            ]
        });
        let convs = parse_conversations(&v);
        assert_eq!(convs.len(), 4);
        assert_eq!(
            convs[0],
            Conversation {
                id: "C1".into(),
                name: "general".into(),
                kind: ConvKind::Channel,
                updated: None
            }
        );
        assert_eq!(
            convs[1],
            Conversation {
                id: "G1".into(),
                name: "priv-team".into(),
                kind: ConvKind::Group,
                updated: None
            }
        );
        // The IM's "name" is the counterpart user id, resolved to a display name later.
        assert_eq!(
            convs[2],
            Conversation { id: "D1".into(), name: "U42".into(), kind: ConvKind::Im, updated: None }
        );
        assert_eq!(
            convs[3],
            Conversation {
                id: "M1".into(),
                name: "mpdm-a-b-1".into(),
                kind: ConvKind::Mpim,
                updated: None
            }
        );
    }

    #[test]
    fn parse_conversations_skips_entries_missing_an_id() {
        let v = serde_json::json!({"channels": [{"name": "no-id"}]});
        assert!(parse_conversations(&v).is_empty());
    }

    #[test]
    fn parse_conversations_reads_updated_when_present_and_none_when_absent() {
        let v = serde_json::json!({
            "channels": [
                {"id": "D1", "user": "U1", "is_im": true, "updated": 1_752_300_000_000_u64},
                {"id": "D2", "user": "U2", "is_im": true}
            ]
        });
        let convs = parse_conversations(&v);
        assert_eq!(convs[0].updated, Some(1_752_300_000_000));
        assert_eq!(convs[1].updated, None);
    }

    #[test]
    fn next_cursor_treats_absent_or_empty_as_the_last_page() {
        assert_eq!(
            next_cursor(&serde_json::json!({"response_metadata": {"next_cursor": "abc"}})),
            Some("abc".to_string())
        );
        assert_eq!(
            next_cursor(&serde_json::json!({"response_metadata": {"next_cursor": ""}})),
            None
        );
        assert_eq!(next_cursor(&serde_json::json!({})), None);
    }

    // ---- history_params (oldest threading, Task 2) -----------------------------------------

    #[test]
    fn history_params_omits_oldest_when_none() {
        assert_eq!(history_params("C1", "50", None, ""), vec![("channel", "C1"), ("limit", "50")]);
    }

    #[test]
    fn history_params_includes_oldest_when_given() {
        assert_eq!(
            history_params("C1", "50", Some("1752300000.000100"), ""),
            vec![("channel", "C1"), ("limit", "50"), ("oldest", "1752300000.000100")]
        );
    }

    #[test]
    fn history_params_includes_the_page_cursor_when_given() {
        assert_eq!(
            history_params("C1", "50", Some("1752300000.000100"), "cur123"),
            vec![
                ("channel", "C1"),
                ("limit", "50"),
                ("oldest", "1752300000.000100"),
                ("cursor", "cur123")
            ]
        );
    }

    // ---- next_history_page (incremental pagination, so a >limit burst leaves no gap) --------

    #[test]
    fn next_history_page_follows_the_cursor_on_an_incremental_fetch() {
        assert_eq!(next_history_page(true, Some("cur".to_string()), 1), Some("cur".to_string()));
    }

    #[test]
    fn next_history_page_never_paginates_the_plain_freshest_window_fetch() {
        // `oldest: None` (backfill / CLI) wants exactly the last-`limit` window, one page.
        assert_eq!(next_history_page(false, Some("cur".to_string()), 1), None);
    }

    #[test]
    fn next_history_page_stops_at_the_defensive_page_cap() {
        assert_eq!(next_history_page(true, Some("cur".to_string()), HISTORY_PAGE_CAP), None);
        assert_eq!(
            next_history_page(true, Some("cur".to_string()), HISTORY_PAGE_CAP - 1),
            Some("cur".to_string())
        );
    }

    #[test]
    fn next_history_page_stops_when_slack_reports_no_more_pages() {
        assert_eq!(next_history_page(true, None, 1), None);
    }

    // ---- replies_params (oldest threading, Task 1) -----------------------------------------

    #[test]
    fn replies_params_omits_oldest_when_none() {
        assert_eq!(
            replies_params("C1", "100.000001", None),
            vec![("channel", "C1"), ("ts", "100.000001")]
        );
    }

    #[test]
    fn replies_params_includes_oldest_when_given() {
        assert_eq!(
            replies_params("C1", "100.000001", Some("100.000002")),
            vec![("channel", "C1"), ("ts", "100.000001"), ("oldest", "100.000002")]
        );
    }

    // ---- history / replies -----------------------------------------------------------------

    #[test]
    fn parse_messages_reads_ts_author_text_and_preserves_slack_ordering() {
        let v = serde_json::json!({
            "messages": [
                {"ts": "1752300000.000200", "user": "U1", "text": "second"},
                {"ts": "1752300000.000100", "user": "U2", "text": "first"}
            ]
        });
        let msgs = parse_messages(&v, "C1");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].ts, "1752300000.000200"); // order preserved, not re-sorted here
        assert_eq!(msgs[0].author, "U1");
        assert_eq!(msgs[0].text, "second");
        assert_eq!(msgs[0].conv, "C1");
        assert!(!msgs[0].edited);
    }

    #[test]
    fn parse_messages_a_thread_root_own_thread_ts_is_none_but_a_reply_carries_the_root() {
        let v = serde_json::json!({
            "messages": [
                {"ts": "100.000001", "user": "U1", "text": "root", "thread_ts": "100.000001"},
                {"ts": "100.000002", "user": "U2", "text": "reply", "thread_ts": "100.000001"}
            ]
        });
        let msgs = parse_messages(&v, "C1");
        assert_eq!(msgs[0].thread_ts, None); // root: thread_ts == its own ts
        assert_eq!(msgs[1].thread_ts, Some("100.000001".to_string()));
    }

    #[test]
    fn parse_messages_reads_reply_count_on_a_root_and_none_when_absent() {
        let v = serde_json::json!({
            "messages": [
                {"ts": "1.1", "user": "U1", "text": "root", "reply_count": 3},
                {"ts": "1.2", "user": "U2", "text": "plain message"}
            ]
        });
        let msgs = parse_messages(&v, "C1");
        assert_eq!(msgs[0].reply_count, Some(3));
        assert_eq!(msgs[1].reply_count, None);
    }

    #[test]
    fn parse_messages_marks_edited_when_the_edited_object_is_present() {
        let v = serde_json::json!({
            "messages": [
                {"ts": "1.1", "user": "U1", "text": "x", "edited": {"user": "U1", "ts": "1.2"}},
                {"ts": "1.2", "user": "U1", "text": "y"}
            ]
        });
        let msgs = parse_messages(&v, "C1");
        assert!(msgs[0].edited);
        assert!(!msgs[1].edited);
    }

    // ---- users.list -------------------------------------------------------------------------

    #[test]
    fn parse_users_prefers_display_name_then_falls_back_to_real_name() {
        let v = serde_json::json!({
            "members": [
                {"id": "U1", "real_name": "Top Real", "profile": {"display_name": "Nick", "real_name": "Profile Real"}},
                {"id": "U2", "real_name": "Top Real", "profile": {"display_name": "", "real_name": "Profile Real"}},
                {"id": "U3", "real_name": "Top Real", "profile": {"display_name": ""}},
                {"id": "U4", "profile": {}}
            ]
        });
        let users = parse_users(&v);
        assert_eq!(users[0], ("U1".to_string(), "Nick".to_string()));
        assert_eq!(users[1], ("U2".to_string(), "Profile Real".to_string()));
        assert_eq!(users[2], ("U3".to_string(), "Top Real".to_string()));
        assert_eq!(users[3], ("U4".to_string(), "U4".to_string())); // last resort: the id itself
    }

    // ---- auth.test --------------------------------------------------------------------------

    #[test]
    fn auth_test_response_carries_the_self_user_id() {
        let v = serde_json::json!({"ok": true, "user_id": "U999", "team_id": "T1"});
        assert_eq!(v["user_id"].as_str(), Some("U999"));
    }
}
