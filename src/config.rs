//! The shared plugin configuration boundary.
//!
//! See `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md` (`## Tokens and config`).
//! `channels` is the only required key; every other key has a documented default. Like
//! reviewr, an unknown key or an invalid value fails the *whole* file loudly instead of
//! silently falling back — a typo in `config.toml` should never look like "everything is
//! fine, just using defaults".

use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const PLUGIN_CONFIG_KEYS: [&str; 9] = [
    "channels",
    "dms",
    "keywords",
    "theme",
    "poll_fallback_secs",
    "dm_limit",
    "dm_allow",
    "focus_keywords",
    "lookback_days",
];

/// The default theme name when `config.toml` omits `theme`.
pub const DEFAULT_THEME: &str = "catppuccin";

/// The default polling fallback interval, in seconds, when the socket is unavailable.
pub const DEFAULT_POLL_FALLBACK_SECS: u64 = 30;

/// Valid range for `poll_fallback_secs`.
const POLL_FALLBACK_SECS_RANGE: std::ops::RangeInclusive<u64> = 5..=300;

/// The default cap on subscribed DMs when `config.toml` omits `dm_limit`.
pub const DEFAULT_DM_LIMIT: u32 = 20;

/// Valid range for `dm_limit`; `0` means "no DMs even when `dms = true`".
const DM_LIMIT_RANGE: std::ops::RangeInclusive<u32> = 0..=200;

/// The default look-back horizon, in days, when `config.toml` omits `lookback_days`.
pub const DEFAULT_LOOKBACK_DAYS: u64 = 7;

/// Valid range for `lookback_days`; `0` means unlimited (no look-back horizon at all).
const LOOKBACK_DAYS_RANGE: std::ops::RangeInclusive<u64> = 0..=365;

/// One validated snapshot of `$HERDR_PLUGIN_CONFIG_DIR/config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginConfig {
    channels: Vec<String>,
    dms: bool,
    keywords: Vec<String>,
    theme: String,
    poll_fallback_secs: u64,
    dm_limit: u32,
    dm_allow: Vec<String>,
    focus_keywords: Vec<String>,
    lookback_days: u64,
}

impl PluginConfig {
    /// Subscribed channel names, e.g. `#eng-infra`. Always non-empty and `#`-prefixed;
    /// resolved to Slack channel ids at startup by a later layer.
    pub fn channels(&self) -> &[String] {
        &self.channels
    }

    /// Whether IMs/MPIMs are included alongside `channels`. Defaults to `true`.
    pub fn dms(&self) -> bool {
        self.dms
    }

    /// Extra Mentions-tab triggers, matched case-insensitively. Defaults to empty.
    pub fn keywords(&self) -> &[String] {
        &self.keywords
    }

    /// The palette name (reviewr's theme system). Defaults to `"catppuccin"`.
    pub fn theme(&self) -> &str {
        &self.theme
    }

    /// Seconds between `conversations.history` polls while the socket is unavailable.
    /// Defaults to 30; valid range is `5..=300`.
    pub fn poll_fallback_secs(&self) -> u64 {
        self.poll_fallback_secs
    }

    /// The cap on subscribed DMs (IMs/MPIMs) when `dms = true`: only the `dm_limit` most
    /// recently active ones are subscribed (see `crate::model::resolve_channels`). Defaults to
    /// 20; valid range is `0..=200`, where `0` means no DMs even when `dms = true`.
    pub fn dm_limit(&self) -> u32 {
        self.dm_limit
    }

    /// DM/MPIM counterpart names (Slack usernames or display names) that are always
    /// subscribed regardless of `dm_limit`, matched exactly and case-insensitively against
    /// the conversation's resolved name (see `crate::model::resolve_channels`). Defaults to
    /// empty. Any non-empty string is allowed — no format restriction beyond that, since
    /// these are free-form Slack display names, not `#`-prefixed channel names.
    pub fn dm_allow(&self) -> &[String] {
        &self.dm_allow
    }

    /// Focus-mode triggers (spec §3), matched case-insensitively as a substring, same rule as
    /// `keywords` — but a *distinct* key, kept deliberately separate: `keywords` says "notify
    /// me" (Mentions tab), `focus_keywords` says "narrow my attention" (Focus view); conflating
    /// them would mean turning one on always affects the other. Defaults to empty.
    pub fn focus_keywords(&self) -> &[String] {
        &self.focus_keywords
    }

    /// How many days back any history fetch may reach: startup backfill drops messages older
    /// than this horizon, and the incremental poll/catch-up/DM-scan paths clamp their `oldest`
    /// to it — bounding both how much history the pane retains attention on and how many
    /// paginated requests a large gap can cost (the *depth* companion to the request-budget
    /// *rate* cap; especially important when the Slack app's rate-limit pool is shared with
    /// other consumers). Defaults to 7; valid range is `0..=365`, where `0` means unlimited.
    pub fn lookback_days(&self) -> u64 {
        self.lookback_days
    }
}

/// A whole-file configuration failure. It keeps the path in the value so every entry point
/// can show the same actionable diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginConfigError {
    path: PathBuf,
    detail: String,
}

impl PluginConfigError {
    fn new(path: &Path, detail: impl Into<String>) -> Self {
        Self { path: path.to_owned(), detail: detail.into() }
    }
}

impl fmt::Display for PluginConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config {}: {}", self.path.display(), self.detail)
    }
}

impl std::error::Error for PluginConfigError {}

/// Read one plugin config snapshot from the process environment. `$HERDR_PLUGIN_CONFIG_DIR`
/// is always set when running inside herdr (`src/main.rs` refuses to start otherwise), so an
/// unset directory here is a caller/test error, not a standalone mode.
pub fn plugin_config() -> Result<PluginConfig, PluginConfigError> {
    let dir = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").ok_or_else(|| {
        PluginConfigError::new(
            Path::new("config.toml"),
            "read failed: HERDR_PLUGIN_CONFIG_DIR is not set",
        )
    })?;
    plugin_config_in(dir)
}

/// Read one plugin config snapshot from `<dir>/config.toml`.
pub fn plugin_config_in(dir: impl AsRef<Path>) -> Result<PluginConfig, PluginConfigError> {
    parse_plugin_config(&dir.as_ref().join("config.toml"))
}

fn parse_plugin_config(path: &Path) -> Result<PluginConfig, PluginConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(PluginConfigError::new(path, format!("read failed: {error}")));
        }
    };
    let table: toml::Table = text.parse().map_err(|error: toml::de::Error| {
        PluginConfigError::new(path, format!("syntax error: {}", error.message()))
    })?;
    if let Some(key) = table.keys().find(|key| !PLUGIN_CONFIG_KEYS.contains(&key.as_str())) {
        return Err(PluginConfigError::new(
            path,
            format!("unknown key {key:?}; expected one of {}", PLUGIN_CONFIG_KEYS.join(", ")),
        ));
    }

    let channels = parse_channels(path, table.get("channels"))?;

    let mut dms = true;
    if let Some(value) = table.get("dms") {
        dms = value.as_bool().ok_or_else(|| value_error(path, "dms", "a boolean"))?;
    }

    let mut keywords: Vec<String> = Vec::new();
    if let Some(value) = table.get("keywords") {
        let expected = "an array of strings";
        let values = value.as_array().ok_or_else(|| value_error(path, "keywords", expected))?;
        keywords = values
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| value_error(path, "keywords", expected))?;
    }

    let mut theme = DEFAULT_THEME.to_owned();
    if let Some(value) = table.get("theme") {
        let name = string_value(path, "theme", value, "a theme name")?;
        name.clone_into(&mut theme);
    }

    let mut poll_fallback_secs = DEFAULT_POLL_FALLBACK_SECS;
    if let Some(value) = table.get("poll_fallback_secs") {
        let expected = "an integer in 5..=300";
        let raw =
            value.as_integer().ok_or_else(|| value_error(path, "poll_fallback_secs", expected))?;
        let secs =
            u64::try_from(raw).map_err(|_| value_error(path, "poll_fallback_secs", expected))?;
        if !POLL_FALLBACK_SECS_RANGE.contains(&secs) {
            return Err(value_error(path, "poll_fallback_secs", expected));
        }
        poll_fallback_secs = secs;
    }

    let mut dm_limit = DEFAULT_DM_LIMIT;
    if let Some(value) = table.get("dm_limit") {
        let expected = "an integer in 0..=200";
        let raw = value.as_integer().ok_or_else(|| value_error(path, "dm_limit", expected))?;
        let limit = u32::try_from(raw).map_err(|_| value_error(path, "dm_limit", expected))?;
        if !DM_LIMIT_RANGE.contains(&limit) {
            return Err(value_error(path, "dm_limit", expected));
        }
        dm_limit = limit;
    }

    let mut dm_allow: Vec<String> = Vec::new();
    if let Some(value) = table.get("dm_allow") {
        let expected = "an array of non-empty strings";
        let values = value.as_array().ok_or_else(|| value_error(path, "dm_allow", expected))?;
        dm_allow = values
            .iter()
            .map(|value| value.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| value_error(path, "dm_allow", expected))?;
    }

    let mut focus_keywords: Vec<String> = Vec::new();
    if let Some(value) = table.get("focus_keywords") {
        let expected = "an array of strings";
        let values =
            value.as_array().ok_or_else(|| value_error(path, "focus_keywords", expected))?;
        focus_keywords = values
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| value_error(path, "focus_keywords", expected))?;
    }

    let mut lookback_days = DEFAULT_LOOKBACK_DAYS;
    if let Some(value) = table.get("lookback_days") {
        let expected = "an integer in 0..=365 (0 means unlimited)";
        let raw = value.as_integer().ok_or_else(|| value_error(path, "lookback_days", expected))?;
        let days = u64::try_from(raw).map_err(|_| value_error(path, "lookback_days", expected))?;
        if !LOOKBACK_DAYS_RANGE.contains(&days) {
            return Err(value_error(path, "lookback_days", expected));
        }
        lookback_days = days;
    }

    Ok(PluginConfig {
        channels,
        dms,
        keywords,
        theme,
        poll_fallback_secs,
        dm_limit,
        dm_allow,
        focus_keywords,
        lookback_days,
    })
}

fn parse_channels(
    path: &Path,
    value: Option<&toml::Value>,
) -> Result<Vec<String>, PluginConfigError> {
    let expected = "a required non-empty array of channel names starting with '#'";
    let Some(value) = value else {
        return Err(value_error(path, "channels", expected));
    };
    let Some(values) = value.as_array() else {
        return Err(value_error(path, "channels", expected));
    };
    if values.is_empty() {
        return Err(value_error(path, "channels", expected));
    }
    let mut channels = Vec::with_capacity(values.len());
    for value in values {
        let Some(channel) = value.as_str() else {
            return Err(value_error(path, "channels", expected));
        };
        if !channel.starts_with('#') {
            return Err(value_error(path, "channels", expected));
        }
        channels.push(channel.to_owned());
    }
    Ok(channels)
}

fn string_value<'a>(
    path: &Path,
    key: &str,
    value: &'a toml::Value,
    expected: &str,
) -> Result<&'a str, PluginConfigError> {
    value.as_str().ok_or_else(|| value_error(path, key, expected))
}

fn value_error(path: &Path, key: &str, expected: &str) -> PluginConfigError {
    PluginConfigError::new(path, format!("invalid value for `{key}`; expected {expected}"))
}

#[cfg(test)]
mod tests {
    use super::PluginConfig;

    fn write(dir: &std::path::Path, text: &str) {
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    #[test]
    fn missing_file_fails_because_channels_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("channels"), "{error}");
    }

    #[test]
    fn defaults_when_only_channels_given() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "channels = [\"#eng-infra\"]\n");
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.channels(), ["#eng-infra"]);
        assert!(config.dms());
        assert!(config.keywords().is_empty());
        assert_eq!(config.theme(), "catppuccin");
        assert_eq!(config.poll_fallback_secs(), 30);
        assert_eq!(config.dm_limit(), 20);
        assert!(config.dm_allow().is_empty());
        assert!(config.focus_keywords().is_empty());
        assert_eq!(config.lookback_days(), 7);
    }

    #[test]
    fn reads_complete_valid_file_as_one_value() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            concat!(
                "channels = [\"#eng-infra\", \"#releases\"]\n",
                "dms = false\n",
                "keywords = [\"deploy\", \"oncall\"]\n",
                "theme = \"tokyo-night\"\n",
                "poll_fallback_secs = 45\n",
                "dm_limit = 15\n",
                "dm_allow = [\"alice\", \"Bob Smith\"]\n",
                "focus_keywords = [\"incident\", \"p1\"]\n",
                "lookback_days = 14\n",
            ),
        );
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.channels(), ["#eng-infra", "#releases"]);
        assert!(!config.dms());
        assert_eq!(config.keywords(), ["deploy", "oncall"]);
        assert_eq!(config.theme(), "tokyo-night");
        assert_eq!(config.poll_fallback_secs(), 45);
        assert_eq!(config.dm_limit(), 15);
        assert_eq!(config.dm_allow(), ["alice", "Bob Smith"]);
        assert_eq!(config.focus_keywords(), ["incident", "p1"]);
        assert_eq!(config.lookback_days(), 14);
    }

    #[test]
    fn focus_keywords_defaults_empty_and_is_distinct_from_keywords() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "channels = [\"#a\"]\nkeywords = [\"deploy\"]\n");
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.keywords(), ["deploy"]);
        assert!(config.focus_keywords().is_empty());
        write(
            dir.path(),
            "channels = [\"#a\"]\nkeywords = [\"deploy\"]\nfocus_keywords = [\"incident\"]\n",
        );
        let config = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(config.keywords(), ["deploy"]);
        assert_eq!(config.focus_keywords(), ["incident"]);
    }

    #[test]
    fn dm_allow_defaults_empty_and_accepts_any_non_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "channels = [\"#a\"]\n");
        assert!(super::plugin_config_in(dir.path()).unwrap().dm_allow().is_empty());
        write(dir.path(), "channels = [\"#a\"]\ndm_allow = [\"weird name #1!\"]\n");
        assert_eq!(super::plugin_config_in(dir.path()).unwrap().dm_allow(), ["weird name #1!"]);
    }

    #[test]
    fn unknown_key_fails_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(dir.path(), "channels = [\"#eng-infra\"]\nbogus = 1\n");
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains(path.to_str().unwrap()));
        assert!(error.contains("unknown key \"bogus\""));
    }

    #[test]
    fn missing_channels_fails() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "dms = false\n");
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("channels"), "{error}");
    }

    #[test]
    fn empty_channels_array_fails() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "channels = []\n");
        assert!(super::plugin_config_in(dir.path()).is_err());
    }

    #[test]
    fn channel_missing_hash_prefix_fails() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "channels = [\"eng-infra\"]\n");
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("channels"), "{error}");
    }

    #[test]
    fn every_invalid_value_fails_instead_of_falling_back() {
        let cases = [
            ("channels = [\"eng-infra\"]\n", "channels"),
            ("channels = [1]\n", "channels"),
            ("channels = \"#eng-infra\"\n", "channels"),
            ("channels = [\"#a\"]\ndms = \"yes\"\n", "dms"),
            ("channels = [\"#a\"]\nkeywords = \"deploy\"\n", "keywords"),
            ("channels = [\"#a\"]\nkeywords = [1]\n", "keywords"),
            ("channels = [\"#a\"]\ntheme = 1\n", "theme"),
            ("channels = [\"#a\"]\npoll_fallback_secs = \"30\"\n", "poll_fallback_secs"),
            ("channels = [\"#a\"]\npoll_fallback_secs = 3\n", "poll_fallback_secs"),
            ("channels = [\"#a\"]\npoll_fallback_secs = 301\n", "poll_fallback_secs"),
            ("channels = [\"#a\"]\ndm_limit = \"20\"\n", "dm_limit"),
            ("channels = [\"#a\"]\ndm_limit = -1\n", "dm_limit"),
            ("channels = [\"#a\"]\ndm_limit = 201\n", "dm_limit"),
            ("channels = [\"#a\"]\ndm_allow = \"alice\"\n", "dm_allow"),
            ("channels = [\"#a\"]\ndm_allow = [1]\n", "dm_allow"),
            ("channels = [\"#a\"]\ndm_allow = [\"\"]\n", "dm_allow"),
            ("channels = [\"#a\"]\nfocus_keywords = \"incident\"\n", "focus_keywords"),
            ("channels = [\"#a\"]\nfocus_keywords = [1]\n", "focus_keywords"),
            ("channels = [\"#a\"]\nlookback_days = \"7\"\n", "lookback_days"),
            ("channels = [\"#a\"]\nlookback_days = -1\n", "lookback_days"),
            ("channels = [\"#a\"]\nlookback_days = 366\n", "lookback_days"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (text, key) in cases {
            std::fs::write(&path, text).unwrap();
            let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
            assert!(error.contains(key), "{text}: {error}");
            assert!(error.contains("expected") || error.contains("required"), "{text}: {error}");
        }
    }

    #[test]
    fn poll_fallback_secs_boundary_values_are_valid() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "channels = [\"#a\"]\npoll_fallback_secs = 5\n");
        assert_eq!(super::plugin_config_in(dir.path()).unwrap().poll_fallback_secs(), 5);
        write(dir.path(), "channels = [\"#a\"]\npoll_fallback_secs = 300\n");
        assert_eq!(super::plugin_config_in(dir.path()).unwrap().poll_fallback_secs(), 300);
    }

    #[test]
    fn dm_limit_boundary_values_are_valid_and_zero_disables_dms_without_disabling_channels() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "channels = [\"#a\"]\ndm_limit = 0\n");
        assert_eq!(super::plugin_config_in(dir.path()).unwrap().dm_limit(), 0);
        write(dir.path(), "channels = [\"#a\"]\ndm_limit = 200\n");
        assert_eq!(super::plugin_config_in(dir.path()).unwrap().dm_limit(), 200);
    }

    #[test]
    fn lookback_days_boundary_values_are_valid_and_zero_means_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "channels = [\"#a\"]\nlookback_days = 0\n");
        assert_eq!(super::plugin_config_in(dir.path()).unwrap().lookback_days(), 0);
        write(dir.path(), "channels = [\"#a\"]\nlookback_days = 365\n");
        assert_eq!(super::plugin_config_in(dir.path()).unwrap().lookback_days(), 365);
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_config_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config.toml")).unwrap();
        let error = super::plugin_config_in(dir.path()).unwrap_err().to_string();
        assert!(error.contains("read failed"));
        assert!(error.contains("config.toml"));
    }

    #[test]
    fn equality_is_field_wise() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "channels = [\"#a\"]\n");
        let a: PluginConfig = super::plugin_config_in(dir.path()).unwrap();
        let b = super::plugin_config_in(dir.path()).unwrap();
        assert_eq!(a, b);
    }
}
