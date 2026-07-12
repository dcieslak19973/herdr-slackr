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
use std::time::Duration;

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
fn events_api(event: &Value) -> Vec<SocketEvent> {
    let conv = event["channel"].as_str().unwrap_or_default().to_string();
    match event["subtype"].as_str() {
        None | Some("bot_message") => vec![SocketEvent::Message(message_from(event, &conv))],
        Some("message_changed") => {
            vec![SocketEvent::Changed(message_from(&event["message"], &conv))]
        }
        Some("message_deleted") => {
            let ts = event["deleted_ts"].as_str().unwrap_or_default().to_string();
            vec![SocketEvent::Deleted { conv, ts }]
        }
        Some(_) => Vec::new(),
    }
}

/// Build a [`Message`] from one event-shaped JSON object (top-level `events_api` event, or
/// the nested `message` object of a `message_changed`), using `conv` as the conversation id
/// (the envelope's `channel`, not necessarily present on the nested object).
fn message_from(v: &Value, conv: &str) -> Message {
    Message {
        conv: conv.to_string(),
        ts: v["ts"].as_str().unwrap_or_default().to_string(),
        thread_ts: v["thread_ts"].as_str().map(str::to_string),
        author: v["user"].as_str().unwrap_or_default().to_string(),
        text: v["text"].as_str().unwrap_or_default().to_string(),
        edited: !v["edited"].is_null(),
    }
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
        match connect_and_pump(&app_token, &tx, &cancelled) {
            Ok(()) => {}
            Err(reason) => {
                let _ = tx.send(SocketEvent::Down(reason));
            }
        }
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let secs = backoff_secs(attempt, default_jitter);
        attempt = attempt.saturating_add(1);
        std::thread::sleep(Duration::from_secs(secs));
    }
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
/// `cancelled`. Returns `Ok(())` on a clean `disconnect`/cancel, `Err(reason)` on any failure.
fn connect_and_pump(
    app_token: &str,
    tx: &Sender<SocketEvent>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let url = crate::rest::connections_open(app_token, cancelled)
        .map_err(|e| format!("connections_open failed: {e:?}"))?;
    let stream = connect_tls(&url)?;
    let (mut ws, _response) = tungstenite::client::client_with_config(&url, stream, None)
        .map_err(|e| format!("websocket handshake failed: {e}"))?;

    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let msg = ws.read().map_err(|e| format!("read failed: {e}"))?;
        let tungstenite::Message::Text(text) = msg else {
            continue;
        };
        let (events, ack) = handle_frame(&text);
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
            })]
        );
        assert_eq!(ack, Some("x".to_string()));
    }

    #[test]
    fn events_api_bot_message_maps_to_message_too() {
        let frame = r#"{"envelope_id":"y","type":"events_api","payload":{"event":
            {"type":"message","subtype":"bot_message","channel":"C1","ts":"1.2","text":"hi"}}}"#;
        let (events, ack) = handle_frame(frame);
        assert!(matches!(events.as_slice(), [SocketEvent::Message(_)]));
        assert_eq!(ack, Some("y".to_string()));
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
            })]
        );
        assert_eq!(ack, Some("z".to_string()));
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
