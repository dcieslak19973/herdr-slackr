//! Sidebar badge: report the unread-mention count onto this pane's herdr sidebar row via
//! `herdr pane report-metadata` (herdr >= 0.7.4). See
//! `docs/superpowers/specs/2026-07-23-sidebar-badge-design.md` and `specs/pane.md`
//! §Nav presence. On older herdr the call fails once, logs once, and the reporter stays
//! silent for the rest of the session — the badge is decoration, never function.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    let title = if unread > 0 { format!("slack ({unread})") } else { "slack".to_string() };
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

/// Reports `(unread, health)` onto this pane's sidebar row, at most once per change.
///
/// Failure latch (spec §Error handling): the CLI thread sets `failed` on any non-success
/// (old herdr rejecting `--token`, missing binary, no server); the next `report` call then
/// writes one plugin-log line and the reporter stays disabled for the session — the
/// plausible causes don't heal mid-run, and retries would only spam a shared machine's log.
#[derive(Debug)]
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
                let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
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

#[cfg(test)]
mod tests {
    use super::{LinkHealth, Reporter, argv};

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
        assert_eq!(argv("p", 1, LinkHealth::Polling).last().unwrap(), "slack_link=polling");
    }

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
}
