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

/// The resolved token pair. `app` is the `xapp-…` app-level token (Socket Mode); `user` is
/// the `xoxp-…` user OAuth token (Web API).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub app: String,
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

/// Resolve both tokens from `env` first, falling back to `<dir>/tokens.toml`. `env` is an
/// injected lookup (real code passes `std::env::var(name).ok()`; tests pass a closure over a
/// fixture map) so tests never mutate real process environment state.
pub fn resolve(dir: &Path, env: impl Fn(&str) -> Option<String>) -> Result<Tokens, TokenError> {
    let app = resolve_token(&env, "SLACK_APP_TOKEN", dir, "app_token", "xapp-")?;
    let user = resolve_token(&env, "SLACK_USER_TOKEN", dir, "user_token", "xoxp-")?;
    Ok(Tokens { app, user })
}

fn resolve_token(
    env: &impl Fn(&str) -> Option<String>,
    env_var: &str,
    dir: &Path,
    toml_key: &str,
    prefix: &str,
) -> Result<String, TokenError> {
    if let Some(value) = env(env_var)
        && !value.is_empty()
    {
        check_prefix(&value, prefix, env_var)?;
        return Ok(value);
    }

    let path = dir.join(TOKENS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(TokenError(format!(
                "{env_var} is not set and {} does not exist; set {env_var} or create it with \
                 `{toml_key} = \"{prefix}...\"`",
                path.display()
            )));
        }
        Err(error) => {
            return Err(TokenError(format!("failed to read {}: {error}", path.display())));
        }
    };

    check_permissions(&path)?;

    let table: toml::Table = text.parse().map_err(|error: toml::de::Error| {
        TokenError(format!("{}: syntax error: {}", path.display(), error.message()))
    })?;

    let Some(value) = table.get(toml_key) else {
        return Err(TokenError(format!(
            "{env_var} is not set and {} has no `{toml_key}`; set {env_var} or add \
             `{toml_key} = \"{prefix}...\"` to {}",
            path.display(),
            path.display()
        )));
    };
    let Some(token) = value.as_str() else {
        return Err(TokenError(format!("{}: `{toml_key}` must be a string", path.display())));
    };
    check_prefix(token, prefix, toml_key)?;
    Ok(token.to_owned())
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
        assert_eq!(tokens.app, "xapp-env");
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
        assert_eq!(tokens.app, "xapp-file");
        assert_eq!(tokens.user, "xoxp-file");
    }

    #[test]
    fn missing_both_names_both_sources() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve(dir.path(), env_from(&[])).unwrap_err().0;
        assert!(error.contains("SLACK_APP_TOKEN"), "{error}");
        assert!(error.contains("tokens.toml"), "{error}");
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
