//! A shared on-disk cache for `users.list`: pane build and every CLI invocation re-fetch the
//! whole workspace member directory otherwise, which is one of the four causes of rate-limit
//! hammering this hardening pass targets (design doc §4). Persists to
//! `$HERDR_PLUGIN_STATE_DIR/users.json` (or, when that env is unset — the CLI's standalone
//! case — the same `~/.local/state/...` layout [`config`](crate::config) derives its own
//! fallback from) with a fetched-at stamp; a read younger than [`TTL_SECS`] (24h) is served
//! from disk, else the caller refetches and rewrites. The cache holds only public directory
//! data (id → display name), but file mode is still `0600` on Unix for consistency with the
//! rest of this crate's on-disk state. Every write is best-effort: an unwritable state dir
//! degrades to a per-process fetch with a `logln!` line, never a hard failure — a debug aid,
//! not a product dependency.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::rest::{Rest, RestError};

/// How long a cached fetch stays fresh before a caller must refetch.
const TTL_SECS: u64 = 24 * 60 * 60;

const CACHE_FILE: &str = "users.json";

/// `<home>/.local/state/herdr/plugins/dcieslak19973.slackr`, the CLI's home-relative fallback
/// when `HERDR_PLUGIN_STATE_DIR` is unset — the state-dir counterpart to `cli::fallback_dir`'s
/// config-dir layout.
const HOME_FALLBACK_REL: &str = ".local/state/herdr/plugins/dcieslak19973.slackr";

/// Resolve the plugin state directory: `HERDR_PLUGIN_STATE_DIR` (via `env_fn`) when set, else
/// `<home>/.local/state/herdr/plugins/dcieslak19973.slackr` using `home_fn`. `None` when
/// neither source is available — callers treat that as "no cache, no store", not an error (the
/// whole point of a best-effort cache is that its absence never blocks anything). Pure and
/// closure-injected, mirroring `cli::config_dir`'s testable shape.
pub fn state_dir(
    env_fn: impl Fn(&str) -> Option<String>,
    home_fn: impl Fn() -> Option<String>,
) -> Option<PathBuf> {
    if let Some(dir) = env_fn("HERDR_PLUGIN_STATE_DIR") {
        return Some(PathBuf::from(dir));
    }
    home_fn().map(|home| Path::new(&home).join(HOME_FALLBACK_REL))
}

/// The current wall-clock time as epoch seconds, `0` on a clock error (pre-epoch system clock)
/// rather than panicking — this is a freshness heuristic, not a correctness-critical value.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Read `<state_dir>/users.json` and return its users when the fetched-at stamp is younger
/// than [`TTL_SECS`] as of `now`. `None` on any miss: file absent, unreadable, malformed, or
/// stale — the caller's uniform "go fetch" signal, so it never needs to distinguish why.
fn load(state_dir: &Path, now: u64) -> Option<Vec<(String, String)>> {
    let text = std::fs::read_to_string(state_dir.join(CACHE_FILE)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let fetched_at = v["fetched_at"].as_u64()?;
    if now.saturating_sub(fetched_at) > TTL_SECS {
        return None;
    }
    let users = v["users"]
        .as_array()?
        .iter()
        .filter_map(|pair| {
            let pair = pair.as_array()?;
            let id = pair.first()?.as_str()?.to_string();
            let name = pair.get(1)?.as_str()?.to_string();
            Some((id, name))
        })
        .collect();
    Some(users)
}

/// Write `users` to `<state_dir>/users.json` stamped with `now`, creating the directory if
/// needed. Best-effort: any I/O failure (unwritable state dir, read-only filesystem, …) is
/// logged via `logln!` and otherwise swallowed — a cache miss next time is the only
/// consequence, never a crash or a hard error surfaced to the caller.
fn store(state_dir: &Path, users: &[(String, String)], now: u64) {
    if let Err(error) = std::fs::create_dir_all(state_dir) {
        crate::logln!("users_cache: create_dir_all({}) failed: {error}", state_dir.display());
        return;
    }
    let arr: Vec<Value> = users.iter().map(|(id, name)| serde_json::json!([id, name])).collect();
    let doc = serde_json::json!({"fetched_at": now, "users": arr});
    let path = state_dir.join(CACHE_FILE);
    let written = std::fs::write(&path, doc.to_string());
    if let Err(error) = &written {
        crate::logln!("users_cache: write({}) failed: {error}", path.display());
    }
    #[cfg(unix)]
    if written.is_ok() {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            crate::logln!("users_cache: chmod 0600 {} failed: {error}", path.display());
        }
    }
}

/// The shared entry point pane build and every CLI invocation call instead of `rest::users`
/// directly: serve `state_dir`'s cache when fresh, else fetch fresh from Slack and (best-effort)
/// rewrite the cache for the next caller. `state_dir` is `None` when neither
/// `HERDR_PLUGIN_STATE_DIR` nor a home directory could be resolved ([`state_dir`]'s own
/// contract) — degrading to a per-process fetch with no cache read or write at all, same as
/// before this cache existed.
pub fn users_cached(
    rest: &Rest,
    state_dir: Option<&Path>,
    now: u64,
) -> Result<Vec<(String, String)>, RestError> {
    if let Some(dir) = state_dir {
        if let Some(users) = load(dir, now) {
            crate::logln!("users_cache: hit ({} users, dir {})", users.len(), dir.display());
            return Ok(users);
        }
        crate::logln!("users_cache: miss/stale (dir {}); fetching users.list", dir.display());
    }
    let users = crate::rest::users(rest)?;
    if let Some(dir) = state_dir {
        store(dir, &users, now);
    }
    Ok(users)
}

#[cfg(test)]
mod tests {
    use super::{load, now_secs, state_dir, store};

    // ---- state_dir --------------------------------------------------------------------------

    #[test]
    fn state_dir_env_wins_over_home_fallback() {
        let dir = state_dir(
            |name| (name == "HERDR_PLUGIN_STATE_DIR").then(|| "/env/state".to_string()),
            || Some("/home/dan".to_string()),
        )
        .unwrap();
        assert_eq!(dir, std::path::PathBuf::from("/env/state"));
    }

    #[test]
    fn state_dir_falls_back_to_home_when_env_unset() {
        let dir = state_dir(|_| None, || Some("/home/dan".to_string())).unwrap();
        assert_eq!(
            dir,
            std::path::Path::new("/home/dan")
                .join(".local/state/herdr/plugins/dcieslak19973.slackr")
        );
    }

    #[test]
    fn state_dir_is_none_when_neither_source_is_available() {
        assert_eq!(state_dir(|_| None, || None), None);
    }

    // ---- load / store TTL ---------------------------------------------------------------------

    #[test]
    fn load_returns_none_when_the_cache_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), now_secs()), None);
    }

    #[test]
    fn store_then_load_round_trips_within_the_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let users =
            vec![("U1".to_string(), "Alice".to_string()), ("U2".to_string(), "Bob".to_string())];
        let now = 1_000_000;
        store(dir.path(), &users, now);
        assert_eq!(load(dir.path(), now + 60), Some(users));
    }

    #[test]
    fn load_returns_none_once_the_ttl_has_elapsed() {
        let dir = tempfile::tempdir().unwrap();
        let users = vec![("U1".to_string(), "Alice".to_string())];
        let now = 1_000_000;
        store(dir.path(), &users, now);
        let past_ttl = now + super::TTL_SECS + 1;
        assert_eq!(load(dir.path(), past_ttl), None);
    }

    #[test]
    fn load_returns_none_for_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("users.json"), "not json").unwrap();
        assert_eq!(load(dir.path(), now_secs()), None);
    }

    #[test]
    fn store_creates_missing_state_dir() {
        let base = tempfile::tempdir().unwrap();
        let nested = base.path().join("nested").join("state");
        let users = vec![("U1".to_string(), "Alice".to_string())];
        store(&nested, &users, 1_000_000);
        assert_eq!(load(&nested, 1_000_060), Some(users));
    }

    #[cfg(unix)]
    #[test]
    fn store_sets_0600_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), &[("U1".to_string(), "Alice".to_string())], 1_000_000);
        let mode = std::fs::metadata(dir.path().join("users.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn store_is_best_effort_when_the_state_dir_cannot_be_created() {
        // A regular file where a directory component is expected: `create_dir_all` fails, and
        // `store` must swallow that rather than panicking.
        let base = tempfile::tempdir().unwrap();
        let blocker = base.path().join("blocker");
        std::fs::write(&blocker, "not a dir").unwrap();
        let nested = blocker.join("state");
        store(&nested, &[("U1".to_string(), "Alice".to_string())], 1_000_000);
        assert_eq!(load(&nested, 1_000_060), None);
    }
}
