//! Sidebar badge: report the unread-mention count onto this pane's herdr sidebar row via
//! `herdr pane report-metadata` (herdr >= 0.7.4). See
//! `docs/superpowers/specs/2026-07-23-sidebar-badge-design.md` and `specs/pane.md`
//! §Nav presence. On older herdr the call fails once, logs once, and the reporter stays
//! silent for the rest of the session — the badge is decoration, never function.

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

#[cfg(test)]
mod tests {
    use super::{LinkHealth, argv};

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
}
