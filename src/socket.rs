//! Socket Mode worker: owns the Slack WebSocket on its own thread. See
//! `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md` §Architecture and §Error
//! handling.
//!
//! The module splits into a pure state-machine core (`handle_frame`, `backoff_secs`, fully
//! unit-tested against canned JSON) and a thin integration edge (`run`, the untested edge —
//! house pattern, see the REST layer's `run_tool`) that wires that core to a real TLS
//! WebSocket and a reconnect loop.

use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::model::Message;

/// What the worker sends the app over an mpsc channel.
#[derive(Debug, PartialEq, Eq)]
pub enum SocketEvent {
    Connected,
    /// A message-family event already mapped to the model (message / `message_changed` /
    /// `message_deleted`), with the conversation id and kind hint from the envelope.
    Message(Message),
    Changed(Message),
    Deleted {
        conv: String,
        ts: String,
    },
    /// The socket is down and the worker is backing off; the app should poll.
    Down(String),
}

/// PURE state machine core: given one raw websocket text frame, produce (events to emit, an
/// optional ack `envelope_id` to send back).
///
/// - `hello` → `[Connected]`, no ack (Slack's `hello` carries no `envelope_id`).
/// - `events_api` envelope wrapping a `message`-family event → the mapped event + ack.
/// - `disconnect` → `[Down(reason)]`, no ack (nothing to acknowledge; the connection is
///   already being torn down and the URL is single-use — see `run`'s doc comment).
/// - Unknown envelope type, or an unparseable frame that still carries an `envelope_id` →
///   `[]` + ack anyway, so Slack does not redeliver garbage forever.
/// - Fully unparseable JSON (no `envelope_id` to recover) → `[]`, no ack.
///
/// Event subtypes within `events_api`: absent subtype or `bot_message` → `Message`;
/// `message_changed` → `Changed` (built from the nested `message` object, whose own `ts` is
/// the *original* message's ts, not the envelope wrapper's); `message_deleted` → `Deleted`
/// (`deleted_ts`); any other subtype is ignored (still acked).
#[must_use]
pub fn handle_frame(frame: &str) -> (Vec<SocketEvent>, Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(frame) else {
        return (Vec::new(), None);
    };
    let envelope_id = v["envelope_id"].as_str().map(str::to_string);
    match v["type"].as_str() {
        Some("hello") => (vec![SocketEvent::Connected], None),
        Some("disconnect") => {
            let reason = v["reason"].as_str().unwrap_or("unknown").to_string();
            (vec![SocketEvent::Down(reason)], None)
        }
        Some("events_api") => (events_api(&v["payload"]["event"]), envelope_id),
        _ => (Vec::new(), envelope_id),
    }
}

/// Map one `events_api` payload event to zero-or-one [`SocketEvent`]s, per the subtype rules
/// in [`handle_frame`]'s doc comment.
///
/// Guard: a missing/garbage payload (no `channel`, or no usable `ts` for the subtype — the
/// nested `message.ts` for `message_changed`, `deleted_ts` for `message_deleted`) yields no
/// event at all rather than a phantom empty [`Message`]; the envelope is still acked (by
/// `handle_frame`, since it still carries an `envelope_id`) so Slack does not redeliver it.
fn events_api(event: &Value) -> Vec<SocketEvent> {
    let conv = event["channel"].as_str().unwrap_or_default();
    if conv.is_empty() {
        return Vec::new();
    }
    let conv = conv.to_string();
    match event["subtype"].as_str() {
        None | Some("bot_message") => {
            if event["ts"].as_str().unwrap_or_default().is_empty() {
                return Vec::new();
            }
            vec![SocketEvent::Message(message_from(event, &conv))]
        }
        Some("message_changed") => {
            if event["message"]["ts"].as_str().unwrap_or_default().is_empty() {
                return Vec::new();
            }
            vec![SocketEvent::Changed(message_from(&event["message"], &conv))]
        }
        Some("message_deleted") => {
            let ts = event["deleted_ts"].as_str().unwrap_or_default().to_string();
            if ts.is_empty() {
                return Vec::new();
            }
            vec![SocketEvent::Deleted { conv, ts }]
        }
        Some(_) => Vec::new(),
    }
}

/// Build a [`Message`] from one event-shaped JSON object (top-level `events_api` event, or
/// the nested `message` object of a `message_changed`), using `conv` as the conversation id
/// (the envelope's `channel`, not necessarily present on the nested object).
///
/// `author` is `user` if present, else `bot_id` (Slack's `bot_message` events usually carry
/// `bot_id` and no `user`), else empty.
///
/// `reply_count` is read off the same object when present: Slack sends a `message_changed` event
/// for a thread root whenever its reply count changes (a new reply posted, one deleted, …), with
/// the nested `message` carrying the updated `reply_count` — so this cheaply picks up fresh
/// thread metadata from the live socket path, not only from `history`/`replies` backfill
/// (`crate::app`'s `active_threads`/marker-count logic doc has the rest of that story). A plain
/// `message`/`bot_message` event never carries this field, so it stays `None` there.
fn message_from(v: &Value, conv: &str) -> Message {
    let user = v["user"].as_str().filter(|s| !s.is_empty());
    let bot_id = v["bot_id"].as_str().filter(|s| !s.is_empty());
    let author = user.or(bot_id).unwrap_or_default().to_string();
    let reply_count = v["reply_count"].as_u64().and_then(|n| u32::try_from(n).ok());
    // A live message event usually carries no `reactions` (a just-posted message has none, and
    // Slack doesn't attach them to `message_changed` reliably) — an empty vec here means "not
    // reported", which `app`'s live upsert rules treat as inherit, never as clear.
    let reactions = v["reactions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some((r["name"].as_str()?.to_string(), u32::try_from(r["count"].as_u64()?).ok()?))
        })
        .collect();
    Message {
        conv: conv.to_string(),
        ts: v["ts"].as_str().unwrap_or_default().to_string(),
        thread_ts: v["thread_ts"].as_str().map(str::to_string),
        author,
        text: v["text"].as_str().unwrap_or_default().to_string(),
        edited: !v["edited"].is_null(),
        reply_count,
        reactions,
    }
}

/// Dead-air liveness deadline for the read loop in [`connect_and_pump_inner`]: the longest
/// stretch of nothing-but-`WouldBlock`/`TimedOut` ticks (each one bounded by the TCP read
/// timeout set in [`connect_tls`]) the loop will tolerate before it gives up on the socket and
/// returns an error, so the normal `Down` + reconnect/backoff path can engage.
///
/// This exists because a firewall that silently drops outbound packets (no FIN, no RST) never
/// produces a socket error — `ws.read()` just keeps timing out forever, and without this
/// deadline the loop would sit in that state indefinitely instead of ever emitting `Down`,
/// leaving the polling fallback unable to kick in. 90 seconds is three read-timeout ticks (3 ×
/// the 30s `set_read_timeout` in `connect_tls`): generous enough that a merely slow-but-live
/// connection is never mistaken for dead, since Slack pings a healthy socket far more often
/// than that and tungstenite answers/consumes pings as ordinary reads (each one resets the
/// clock), but short enough that a truly dead connection is caught well within one manual smoke
/// test's patience.
const LIVENESS_DEADLINE: Duration = Duration::from_secs(90);

/// Pure predicate behind the liveness deadline: has more than [`LIVENESS_DEADLINE`] elapsed
/// since the last successfully read frame? Split out from the read loop so the boundary
/// behavior is unit-testable without a real socket — see [`connect_and_pump_inner`] for the
/// wiring.
#[must_use]
fn silence_exceeded(last_frame: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_frame) > LIVENESS_DEADLINE
}

/// Reconnect schedule: attempt n (0-based) → seconds, before jitter: `1, 2, 4, 8, ...` capped
/// at 60, then `jitter` is applied to that base (injected so tests can pin it, e.g. identity
/// or a fixed +25%).
#[must_use]
pub fn backoff_secs(attempt: u32, jitter: impl Fn(u64) -> u64) -> u64 {
    let base = 1u64.checked_shl(attempt).unwrap_or(u64::MAX).min(60);
    jitter(base)
}

/// The worker loop (thin, untested integration edge — house pattern, see the REST layer's
/// `run_tool` doc comment).
///
/// Contract:
/// - Every reconnect — including the very first connect — calls [`crate::rest::connections_open`]
///   fresh. Slack Socket Mode URLs are single-use: a `disconnect` or a dropped connection means
///   the old URL is dead, and reusing it (rather than opening a new one) is a protocol violation,
///   not just wasted effort (spec §Error handling: "Slack rotates socket URLs; never reuse one").
/// - Each `hello`/`events_api`/`disconnect`/unknown frame goes through [`handle_frame`]; every
///   event it returns is forwarded on `tx`, and every `Some(envelope_id)` it returns is written
///   back as a text frame `{"envelope_id":"<id>"}` — this is the *only* place acks are sent, so
///   the ack contract lives entirely in the pure core and is fully unit-tested there.
/// - A `disconnect` frame or any read/write/handshake error ends the current connection: the
///   loop emits `Down(reason)`, sleeps [`backoff_secs`] for the current attempt count (reset to
///   0 on every successful `hello`), and reconnects from scratch (new `connections_open` call,
///   new TLS handshake).
/// - The loop polls `cancelled` between attempts and after every read/write so a shutdown
///   request unblocks promptly rather than waiting out a stalled socket.
///
/// TLS is wired by hand (no bundled-TLS tungstenite feature): a plain `TcpStream` to the host
/// from the wss URL on port 443, wrapped in a `rustls::ClientConnection` built from a root
/// store seeded with `rustls_native_certs::load_native_certs()` (so a corporate MITM CA in the
/// system trust store works) extended with `webpki_roots::TLS_SERVER_ROOTS` as a fallback for
/// hosts the native store doesn't cover, then `rustls::StreamOwned` glues the two into one
/// `Read + Write` that `tungstenite::client_with_config` handshakes the WebSocket protocol
/// over. tungstenite answers Pings automatically on the next `read`/`write` call; this loop
/// calls `flush` after every write so a pong (or an ack) actually reaches the wire promptly
/// rather than sitting in a write buffer.
// The by-value signature is the fixed cross-thread contract (Task 7 does
// `thread::spawn(move || socket::run(app_token, tx, cancelled))`): every argument is moved
// into the new thread, so taking ownership here is correct even though this function itself
// only borrows them internally.
#[allow(clippy::needless_pass_by_value)]
pub fn run(app_token: String, tx: Sender<SocketEvent>, cancelled: Arc<AtomicBool>) {
    let mut attempt: u32 = 0;
    while !cancelled.load(Ordering::Acquire) {
        let (result, had_hello) = connect_and_pump(&app_token, &tx, &cancelled);
        if let Err(reason) = result {
            let _ = tx.send(SocketEvent::Down(reason));
        }
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if had_hello {
            attempt = 0;
        }
        let secs = backoff_secs(attempt, default_jitter);
        attempt = next_attempt(attempt, had_hello);
        std::thread::sleep(Duration::from_secs(secs));
    }
}

/// The attempt counter to carry into the *next* round, given whether the connection that just
/// ended produced a successful `hello` at any point. A `hello` means the session was healthy,
/// so the backoff schedule resets to 0 — this covers both a clean disconnect after a healthy
/// session (reviewer note: should restart promptly, not from the escalated attempt count) and
/// any subsequent failure starting its own fresh doubling schedule. No `hello` means the
/// connection never came up, so the schedule keeps escalating.
#[must_use]
fn next_attempt(prev: u32, had_hello: bool) -> u32 {
    if had_hello { 0 } else { prev.saturating_add(1) }
}

/// `±25%` jitter around `base`, used by [`run`]'s real reconnect schedule (tests pin their own
/// jitter fn instead — see `backoff_secs`'s doc comment).
fn default_jitter(base: u64) -> u64 {
    // A cheap non-cryptographic jitter source: the low bits of the current time. Good enough
    // for spreading reconnect storms; never used by tests, which inject a deterministic fn.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pct = 75 + u64::from(nanos % 51); // 75..=125
    base.saturating_mul(pct) / 100
}

/// One connect-and-pump cycle: open a fresh socket URL, TLS-handshake, WebSocket-handshake,
/// then loop reading frames through [`handle_frame`] until `disconnect`, an error, or
/// `cancelled`.
///
/// Returns `(Ok(()), had_hello)` on a clean `disconnect`/cancel, `(Err(reason), had_hello)` on
/// any failure, where `had_hello` is whether this connection ever received a `hello` frame
/// (regardless of how the cycle ended) — [`run`] uses it to reset the backoff schedule, since
/// a `hello` means the session was healthy at least for a while.
///
/// The underlying TCP read has a 30s timeout (set in [`connect_tls`]); Slack pings far more
/// often than that on a healthy socket, so one timeout tick means only a quiet moment, not a
/// dead connection. On each tick the loop re-checks `cancelled` and reads again — which is what
/// lets `cancelled` actually interrupt a blocked read in a timely fashion rather than waiting
/// out an indefinitely silent socket — *unless* ticks have piled up past [`LIVENESS_DEADLINE`]
/// since the last frame actually read, in which case the silence is presumed to be a dead
/// connection (e.g. a firewall dropping packets with no FIN/RST) and the loop returns an error
/// so the normal `Down` + reconnect/backoff path engages.
fn connect_and_pump(
    app_token: &str,
    tx: &Sender<SocketEvent>,
    cancelled: &AtomicBool,
) -> (Result<(), String>, bool) {
    let mut had_hello = false;
    match connect_and_pump_inner(app_token, tx, cancelled, &mut had_hello) {
        Ok(()) => (Ok(()), had_hello),
        Err(reason) => (Err(reason), had_hello),
    }
}

fn connect_and_pump_inner(
    app_token: &str,
    tx: &Sender<SocketEvent>,
    cancelled: &AtomicBool,
    had_hello: &mut bool,
) -> Result<(), String> {
    let url = crate::rest::connections_open(app_token, cancelled)
        .map_err(|e| format!("connections_open failed: {e:?}"))?;
    let stream = connect_tls(&url)?;
    let (mut ws, _response) = tungstenite::client::client_with_config(&url, stream, None)
        .map_err(|e| format!("websocket handshake failed: {e}"))?;

    let mut last_frame = Instant::now();
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let msg = match ws.read() {
            Ok(msg) => msg,
            Err(tungstenite::Error::Io(ref e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Read timeout tick, not by itself a dead connection — see `connect_and_pump`'s
                // doc comment. But if ticks like this have piled up past `LIVENESS_DEADLINE`
                // since the last frame actually read, the silence is no longer explained by a
                // merely slow socket; treat it as dead so `Down` + reconnect/backoff engages
                // (this is what lets a firewall's silent packet drop — no FIN/RST, no read
                // error — still surface as a failure instead of hanging forever).
                if silence_exceeded(last_frame, Instant::now()) {
                    return Err(format!(
                        "no frames for {}s — connection presumed dead",
                        LIVENESS_DEADLINE.as_secs()
                    ));
                }
                continue;
            }
            Err(e) => return Err(format!("read failed: {e}")),
        };
        last_frame = Instant::now();
        let tungstenite::Message::Text(text) = msg else {
            continue;
        };
        let (events, ack) = handle_frame(&text);
        if events.is_empty() {
            // Unknown/unparseable envelope, or one whose payload had nothing usable — acked
            // (if it carried an envelope_id) and skipped, never a crash. Logged per spec
            // §Error handling; a no-op unless HERDR_PLUGIN_STATE_DIR is set (see `crate::log`).
            let preview: String = text.chars().take(200).collect();
            crate::logln!("socket: skipped frame with no mapped events: {preview}");
        }
        if events.iter().any(|e| matches!(e, SocketEvent::Connected)) {
            *had_hello = true;
        }
        let is_down = events.iter().any(|e| matches!(e, SocketEvent::Down(_)));
        for event in events {
            let _ = tx.send(event);
        }
        if let Some(id) = ack {
            let ack_frame = format!(r#"{{"envelope_id":"{id}"}}"#);
            ws.write(tungstenite::Message::Text(ack_frame.into()))
                .map_err(|e| format!("ack write failed: {e}"))?;
            ws.flush().map_err(|e| format!("ack flush failed: {e}"))?;
        }
        if is_down {
            return Ok(());
        }
    }
}

/// Build the `Read + Write` TLS stream `tungstenite::client_with_config` handshakes over: a
/// plain TCP connection to `url`'s host on 443, wrapped in a `rustls::ClientConnection` — see
/// [`run`]'s doc comment for why the root store mixes native + webpki-roots certs.
///
/// The TCP stream gets a 30s read timeout so a silent connection can't block `ws.read()`
/// forever — see [`connect_and_pump`]'s doc comment for how the read loop treats a timeout as
/// a tick to re-check `cancelled`, not as connection death.
fn connect_tls(
    url: &str,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, String> {
    let host = url::host_from_wss(url).ok_or_else(|| format!("no host in url: {url}"))?;

    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config =
        rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|e| format!("invalid host {host}: {e}"))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("tls setup failed: {e}"))?;
    let sock =
        TcpStream::connect((host.as_str(), 443)).map_err(|e| format!("tcp connect failed: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("set_read_timeout failed: {e}"))?;
    Ok(rustls::StreamOwned::new(conn, sock))
}

/// A tiny scratch module for the one bit of URL parsing `run` needs: pulling the host out of
/// a `wss://host/path` URL without pulling in a full URL-parsing dependency (not in the closed
/// dep list).
mod url {
    pub(super) fn host_from_wss(url: &str) -> Option<String> {
        let rest = url.strip_prefix("wss://").or_else(|| url.strip_prefix("ws://"))?;
        let host = rest.split(['/', ':', '?']).next()?;
        if host.is_empty() { None } else { Some(host.to_string()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- handle_frame: hello ---------------------------------------------------------------

    #[test]
    fn hello_yields_connected_and_no_ack() {
        let (events, ack) = handle_frame(r#"{"type":"hello","num_connections":1}"#);
        assert_eq!(events, vec![SocketEvent::Connected]);
        assert_eq!(ack, None);
    }

    // ---- handle_frame: events_api / message ------------------------------------------------

    #[test]
    fn events_api_plain_message_maps_and_acks() {
        let frame = r#"{"envelope_id":"x","type":"events_api","payload":{"event":
            {"type":"message","channel":"C1","ts":"1.2","user":"U1","text":"hi"}}}"#;
        let (events, ack) = handle_frame(frame);
        assert_eq!(
            events,
            vec![SocketEvent::Message(Message {
                conv: "C1".into(),
                ts: "1.2".into(),
                thread_ts: None,
                author: "U1".into(),
                text: "hi".into(),
                edited: false,
                reply_count: None,
                reactions: Vec::new(),
            })]
        );
        assert_eq!(ack, Some("x".to_string()));
    }

    #[test]
    fn events_api_bot_message_maps_to_message_too() {
        let frame = r#"{"envelope_id":"y","type":"events_api","payload":{"event":
            {"type":"message","subtype":"bot_message","channel":"C1","ts":"1.2","text":"hi",
             "bot_id":"B1"}}}"#;
        let (events, ack) = handle_frame(frame);
        match events.as_slice() {
            [SocketEvent::Message(m)] => assert_eq!(m.author, "B1"),
            other => panic!("expected one Message event, got {other:?}"),
        }
        assert_eq!(ack, Some("y".to_string()));
    }

    // ---- handle_frame: events_api / missing or unusable payload ----------------------------

    #[test]
    fn events_api_missing_payload_yields_no_events_but_still_acks() {
        let (events, ack) = handle_frame(r#"{"envelope_id":"g","type":"events_api","payload":{}}"#);
        assert!(events.is_empty());
        assert_eq!(ack, Some("g".to_string()));
    }

    #[test]
    fn events_api_event_missing_channel_yields_no_events_but_still_acks() {
        let frame = r#"{"envelope_id":"h","type":"events_api","payload":{"event":
            {"type":"message","ts":"1.2","user":"U1","text":"hi"}}}"#;
        let (events, ack) = handle_frame(frame);
        assert!(events.is_empty());
        assert_eq!(ack, Some("h".to_string()));
    }

    #[test]
    fn events_api_event_missing_ts_yields_no_events_but_still_acks() {
        let frame = r#"{"envelope_id":"i","type":"events_api","payload":{"event":
            {"type":"message","channel":"C1","user":"U1","text":"hi"}}}"#;
        let (events, ack) = handle_frame(frame);
        assert!(events.is_empty());
        assert_eq!(ack, Some("i".to_string()));
    }

    #[test]
    fn events_api_message_deleted_missing_deleted_ts_yields_no_events_but_still_acks() {
        let frame = r#"{"envelope_id":"j","type":"events_api","payload":{"event":
            {"type":"message","subtype":"message_deleted","channel":"C1"}}}"#;
        let (events, ack) = handle_frame(frame);
        assert!(events.is_empty());
        assert_eq!(ack, Some("j".to_string()));
    }

    #[test]
    fn events_api_message_changed_maps_the_nested_message_and_keeps_the_envelope_channel() {
        let frame = r#"{"envelope_id":"z","type":"events_api","payload":{"event":
            {"type":"message","subtype":"message_changed","channel":"C1",
             "message":{"ts":"1.2","user":"U1","text":"edited","edited":{"user":"U1","ts":"1.3"}}}}}"#;
        let (events, ack) = handle_frame(frame);
        assert_eq!(
            events,
            vec![SocketEvent::Changed(Message {
                conv: "C1".into(),
                ts: "1.2".into(),
                thread_ts: None,
                author: "U1".into(),
                text: "edited".into(),
                edited: true,
                reply_count: None,
                reactions: Vec::new(),
            })]
        );
        assert_eq!(ack, Some("z".to_string()));
    }

    #[test]
    fn events_api_message_changed_carries_the_nested_messages_reply_count_when_present() {
        // Slack sends message_changed for a thread root whenever its reply_count changes; the
        // nested message object carries the fresh count, which message_from should pick up
        // without waiting on the next history/replies backfill.
        let frame = r#"{"envelope_id":"rc","type":"events_api","payload":{"event":
            {"type":"message","subtype":"message_changed","channel":"C1",
             "message":{"ts":"1.2","user":"U1","text":"root","reply_count":4}}}}"#;
        let (events, ack) = handle_frame(frame);
        match events.as_slice() {
            [SocketEvent::Changed(m)] => assert_eq!(m.reply_count, Some(4)),
            other => panic!("expected one Changed event, got {other:?}"),
        }
        assert_eq!(ack, Some("rc".to_string()));
    }

    #[test]
    fn events_api_message_deleted_maps_conv_and_deleted_ts() {
        let frame = r#"{"envelope_id":"d","type":"events_api","payload":{"event":
            {"type":"message","subtype":"message_deleted","channel":"C1","deleted_ts":"1.2"}}}"#;
        let (events, ack) = handle_frame(frame);
        assert_eq!(events, vec![SocketEvent::Deleted { conv: "C1".into(), ts: "1.2".into() }]);
        assert_eq!(ack, Some("d".to_string()));
    }

    #[test]
    fn events_api_unknown_subtype_is_ignored_but_still_acked() {
        let frame = r#"{"envelope_id":"u","type":"events_api","payload":{"event":
            {"type":"message","subtype":"channel_join","channel":"C1"}}}"#;
        let (events, ack) = handle_frame(frame);
        assert!(events.is_empty());
        assert_eq!(ack, Some("u".to_string()));
    }

    // ---- handle_frame: disconnect -----------------------------------------------------------

    #[test]
    fn disconnect_yields_down_and_no_ack() {
        let (events, ack) = handle_frame(r#"{"type":"disconnect","reason":"refresh_requested"}"#);
        assert_eq!(events, vec![SocketEvent::Down("refresh_requested".to_string())]);
        assert_eq!(ack, None);
    }

    // ---- handle_frame: garbage / unknown -----------------------------------------------------

    #[test]
    fn unparseable_frame_has_no_events_and_no_ack() {
        let (events, ack) = handle_frame("{");
        assert!(events.is_empty());
        assert_eq!(ack, None);
    }

    #[test]
    fn unknown_envelope_type_with_an_id_is_acked_but_produces_no_events() {
        let (events, ack) = handle_frame(r#"{"envelope_id":"q","type":"some_future_type"}"#);
        assert!(events.is_empty());
        assert_eq!(ack, Some("q".to_string()));
    }

    // ---- backoff_secs -----------------------------------------------------------------------

    #[test]
    fn backoff_secs_doubles_with_identity_jitter() {
        assert_eq!(backoff_secs(0, |b| b), 1);
        assert_eq!(backoff_secs(1, |b| b), 2);
        assert_eq!(backoff_secs(5, |b| b), 32);
        assert_eq!(backoff_secs(8, |b| b), 60); // capped
    }

    #[test]
    fn backoff_secs_applies_the_injected_jitter() {
        assert_eq!(backoff_secs(2, |b| b * 10), 40);
    }

    // ---- next_attempt -------------------------------------------------------------------------

    #[test]
    fn next_attempt_increments_when_no_hello_was_seen() {
        assert_eq!(next_attempt(0, false), 1);
        assert_eq!(next_attempt(4, false), 5);
    }

    #[test]
    fn next_attempt_resets_to_zero_once_hello_was_seen() {
        assert_eq!(next_attempt(0, true), 0);
        assert_eq!(next_attempt(7, true), 0);
    }

    // ---- silence_exceeded -----------------------------------------------------------------

    #[test]
    fn silence_exceeded_is_false_before_the_liveness_deadline() {
        let last_frame = std::time::Instant::now();
        let now = last_frame + Duration::from_secs(89);
        assert!(!silence_exceeded(last_frame, now));
    }

    #[test]
    fn silence_exceeded_is_false_exactly_at_the_liveness_deadline() {
        let last_frame = std::time::Instant::now();
        let now = last_frame + LIVENESS_DEADLINE;
        assert!(!silence_exceeded(last_frame, now));
    }

    #[test]
    fn silence_exceeded_is_true_once_past_the_liveness_deadline() {
        let last_frame = std::time::Instant::now();
        let now = last_frame + LIVENESS_DEADLINE + Duration::from_secs(1);
        assert!(silence_exceeded(last_frame, now));
    }

    // ---- url::host_from_wss -------------------------------------------------------------------

    #[test]
    fn host_from_wss_strips_scheme_path_port_and_query() {
        assert_eq!(
            url::host_from_wss("wss://wss-primary.slack.com/link/foo"),
            Some("wss-primary.slack.com".to_string())
        );
        assert_eq!(url::host_from_wss("wss://host:443/path"), Some("host".to_string()));
        assert_eq!(url::host_from_wss("wss://host?x=1"), Some("host".to_string()));
        assert_eq!(url::host_from_wss("not-a-url"), None);
    }
}
