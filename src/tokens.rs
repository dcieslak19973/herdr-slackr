//! Slack token resolution.
//!
//! See `docs/superpowers/specs/2026-07-12-herdr-slackr-design.md` (`## Tokens and config`).
//! Per token, resolution is env first, then `$HERDR_PLUGIN_CONFIG_DIR/tokens.toml`; a
//! present-but-wrong-prefix token is a loud error rather than a silent pass-through, and on
//! Unix a `tokens.toml` readable by group or world is refused with a `chmod 600` remedy —
//! Slack tokens are bearer credentials, so an over-permissioned file is a real incident, not
//! a lint. Error messages name the source (env var or config key) but never the token value:
//! any string derived from user input is deliberately excluded from every `TokenError`.

use std::io::ErrorKind;
use std::path::Path;

/// The resolved tokens. `user` is the `xoxp-…` user OAuth token (Web API) — always required,
/// every read goes through it. `app` is the `xapp-…` app-level token (Socket Mode) — optional:
/// its absence selects **poll-only mode** (no socket worker, permanent polling fallback).
/// That opt-out exists because opening a socket is sometimes actively wrong, not just
/// unnecessary: Socket Mode load-balances events across every open connection *and the pane
/// acks what it receives*, so pointing a second consumer at another service's app makes both
/// silently steal each other's events. A *malformed* app token still fails loud
/// ([`resolve`]'s prefix check) — absence is an opt-out, a typo is a mistake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub app: Option<String>,
    pub user: String,
}

/// A token resolution failure. The message is remedy text only — construction sites in this
/// module never format a candidate token value into it, so it is always safe to print
/// verbatim to a log or the pane's status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenError(pub String);

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TokenError {}

const TOKENS_FILE: &str = "tokens.toml";

/// Resolve the tokens from `env` first, falling back to `<dir>/tokens.toml`. `env` is an
/// injected lookup (real code passes `std::env::var(name).ok()`; tests pass a closure over a
/// fixture map) so tests never mutate real process environment state. The user token is
/// required; the app token's absence resolves to `None` (poll-only mode — see [`Tokens`]).
pub fn resolve(dir: &Path, env: impl Fn(&str) -> Option<String>) -> Result<Tokens, TokenError> {
    let app = lookup_token(&env, "SLACK_APP_TOKEN", dir, "app_token", "xapp-")?;
    let user =
        lookup_token(&env, "SLACK_USER_TOKEN", dir, "user_token", "xoxp-")?.ok_or_else(|| {
            TokenError(format!(
                "SLACK_USER_TOKEN is not set and {} has no `user_token`; set SLACK_USER_TOKEN \
                 or add `user_token = \"xoxp-...\"` to that file",
                dir.join(TOKENS_FILE).display()
            ))
        })?;
    Ok(Tokens { app, user })
}

/// One token's resolution: `Ok(None)` only for genuine *absence* (env unset/empty, and the
/// file missing or lacking the key). Everything a user could plausibly have gotten wrong —
/// unreadable or over-permissioned file, syntax error, non-string value, wrong prefix — is
/// still a loud error, never a silent `None`.
fn lookup_token(
    env: &impl Fn(&str) -> Option<String>,
    env_var: &str,
    dir: &Path,
    toml_key: &str,
    prefix: &str,
) -> Result<Option<String>, TokenError> {
    if let Some(value) = env(env_var)
        && !value.is_empty()
    {
        check_prefix(&value, prefix, env_var)?;
        return Ok(Some(value));
    }

    let path = dir.join(TOKENS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(TokenError(format!("failed to read {}: {error}", path.display())));
        }
    };

    check_permissions(&path)?;

    let table: toml::Table = text.parse().map_err(|error: toml::de::Error| {
        TokenError(format!("{}: syntax error: {}", path.display(), error.message()))
    })?;

    let Some(value) = table.get(toml_key) else {
        return Ok(None);
    };
    let Some(token) = value.as_str() else {
        return Err(TokenError(format!("{}: `{toml_key}` must be a string", path.display())));
    };
    check_prefix(token, prefix, toml_key)?;
    Ok(Some(token.to_owned()))
}

/// Confirms `token` carries the expected Slack prefix without ever putting `token` itself
/// into the returned error.
fn check_prefix(token: &str, prefix: &str, source: &str) -> Result<(), TokenError> {
    if token.starts_with(prefix) {
        Ok(())
    } else {
        Err(TokenError(format!("{source} must be a `{prefix}` token")))
    }
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<(), TokenError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path)
        .map_err(|error| TokenError(format!("failed to stat {}: {error}", path.display())))?;
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(TokenError(format!(
            "{} is readable by group or world; run `chmod 600 {}`",
            path.display(),
            path.display()
        )));
    }
    Ok(())
}

/// Windows has no POSIX permission bits to check; this is a deliberate no-op so the same
/// call site works on every dev/CI platform. The `Result` return type is kept identical to
/// the Unix version even though this arm never fails, so callers don't need a cfg split.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn check_permissions(_path: &Path) -> Result<(), TokenError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn env_wins_over_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tokens.toml"),
            "app_token = \"xapp-file\"\nuser_token = \"xoxp-file\"\n",
        )
        .unwrap();
        let env = env_from(&[("SLACK_APP_TOKEN", "xapp-env"), ("SLACK_USER_TOKEN", "xoxp-env")]);
        let tokens = resolve(dir.path(), env).unwrap();
        assert_eq!(tokens.app.as_deref(), Some("xapp-env"));
        assert_eq!(tokens.user, "xoxp-env");
    }

    #[test]
    fn file_used_when_env_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.toml");
        std::fs::write(&path, "app_token = \"xapp-file\"\nuser_token = \"xoxp-file\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let tokens = resolve(dir.path(), env_from(&[])).unwrap();
        assert_eq!(tokens.app.as_deref(), Some("xapp-file"));
        assert_eq!(tokens.user, "xoxp-file");
    }

    #[test]
    fn missing_user_token_names_both_sources() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve(dir.path(), env_from(&[])).unwrap_err().0;
        assert!(error.contains("SLACK_USER_TOKEN"), "{error}");
        assert!(error.contains("tokens.toml"), "{error}");
    }

    // ---- optional app token (poll-only mode): the xapp token exists only for Socket Mode, and
    // there are real deployments where opening a socket is wrong — e.g. the only approvable app
    // is another service's, and Socket Mode load-balances (and acks!) events across every open
    // connection, so a second consumer silently steals the first one's events. ------------------

    #[test]
    fn absent_app_token_resolves_to_none_when_the_user_token_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_from(&[("SLACK_USER_TOKEN", "xoxp-env")]);
        let tokens = resolve(dir.path(), env).unwrap();
        assert_eq!(tokens.app, None, "no app token means poll-only mode, not an error");
        assert_eq!(tokens.user, "xoxp-env");
    }

    #[test]
    fn file_with_only_a_user_token_resolves_to_poll_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.toml");
        std::fs::write(&path, "user_token = \"xoxp-file\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let tokens = resolve(dir.path(), env_from(&[])).unwrap();
        assert_eq!(tokens.app, None);
        assert_eq!(tokens.user, "xoxp-file");
    }

    #[test]
    fn a_present_but_wrong_prefix_app_token_still_fails_loud() {
        // A typo'd xapp token must never silently degrade to poll-only mode — absence is an
        // opt-out, malformation is a mistake.
        let dir = tempfile::tempdir().unwrap();
        let env = env_from(&[("SLACK_APP_TOKEN", "xoxb-wrong"), ("SLACK_USER_TOKEN", "xoxp-env")]);
        let error = resolve(dir.path(), env).unwrap_err().0;
        assert!(error.contains("xapp-"), "{error}");
    }

    #[test]
    fn wrong_prefix_names_expected_prefix_and_never_echoes_value() {
        let dir = tempfile::tempdir().unwrap();
        let env =
            env_from(&[("SLACK_APP_TOKEN", "xapp-good"), ("SLACK_USER_TOKEN", "xoxb-fake-1234")]);
        let error = resolve(dir.path(), env).unwrap_err().0;
        assert!(error.contains("xoxp-"), "{error}");
        assert!(!error.contains("xoxb"), "{error}");
        assert!(!error.contains("fake-1234"), "{error}");
    }

    #[test]
    #[cfg(unix)]
    fn group_or_world_readable_tokens_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.toml");
        std::fs::write(&path, "app_token = \"xapp-file\"\nuser_token = \"xoxp-file\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = resolve(dir.path(), env_from(&[])).unwrap_err().0;
        assert!(error.contains("chmod 600"), "{error}");
    }
}
