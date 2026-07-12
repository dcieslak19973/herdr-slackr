//! Optional event log for live debugging, and the socket worker's skipped-frame trail (spec
//! §Error handling: unknown/unparseable envelopes are "acked and skipped with a log line").
//!
//! When `$HERDR_PLUGIN_STATE_DIR` names a directory, the binary appends one timestamped line
//! per skipped socket frame (and any other call site that opts into `logln!`) to
//! `<dir>/slackr.log`. Unset is the default and makes every call site a no-op (the `logln!`
//! macro skips formatting), so this is never product behavior. Adapted from
//! `herdr-reviewr`'s `src/log.rs` (env var swapped for the plugin state directory, since a
//! non-interactive plugin has no natural place to point a debug env var at other than the
//! directory herdr already gives it).

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

const LOG_FILE: &str = "slackr.log";

/// Open the log sink at `$HERDR_PLUGIN_STATE_DIR/slackr.log` if the directory is set. Call
/// once at startup.
pub fn init() {
    SINK.get_or_init(|| {
        let dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR")?;
        let path = std::path::Path::new(&dir).join(LOG_FILE);
        OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
    });
}

/// Whether a log sink is open.
pub fn enabled() -> bool {
    matches!(SINK.get(), Some(Some(_)))
}

/// Append one formatted line, prefixed with epoch milliseconds.
pub fn write(line: &str) {
    if let Some(Some(sink)) = SINK.get()
        && let Ok(mut file) = sink.lock()
    {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let _ = writeln!(file, "{ts} {line}");
    }
}

/// Log a formatted line when logging is enabled; otherwise do nothing.
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {
        if $crate::log::enabled() {
            $crate::log::write(&format!($($arg)*));
        }
    };
}
