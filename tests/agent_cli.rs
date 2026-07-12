//! Integration tests for the agent-facing `mentions`/`feed`/`skill-path`/`skill-install`
//! subcommands, spawning the real binary. `mentions`/`feed`'s REST-touching paths are never
//! driven against live Slack here — a config+tokens fixture pointing at a tempdir gets the
//! discovery layer past config/token resolution, and the assertion is that the resulting
//! failure is a Slack/curl error (proving discovery worked), not a config error.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_herdr-slackr")
}

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ---- config discovery ---------------------------------------------------------------------

#[test]
fn no_config_anywhere_exits_1_naming_both_candidate_paths() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["mentions"])
        .env_remove("HERDR_PLUGIN_CONFIG_DIR")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .output()
        .expect("spawn herdr-slackr");

    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("HERDR_PLUGIN_CONFIG_DIR"), "names the env var: {err}");
    assert!(
        err.contains(".config") && err.contains("dcieslak19973.slackr"),
        "names the fallback path: {err}"
    );
}

/// A config+tokens fixture directory: enough for discovery to succeed, so any subsequent
/// failure has to come from the REST layer (proving discovery worked) rather than from a
/// config/token resolution failure.
fn rest_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "channels = [\"#eng-infra\"]\n").unwrap();
    std::fs::write(
        dir.path().join("tokens.toml"),
        "app_token = \"xapp-fake-1234\"\nuser_token = \"xoxp-fake-1234\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.path().join("tokens.toml"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    dir
}

#[test]
fn mentions_with_valid_config_but_a_fake_token_fails_at_the_rest_layer_not_config() {
    let fixture = rest_fixture();
    let out = Command::new(bin())
        .args(["mentions"])
        .env("HERDR_PLUGIN_CONFIG_DIR", fixture.path())
        .env_remove("SLACK_APP_TOKEN")
        .env_remove("SLACK_USER_TOKEN")
        .output()
        .expect("spawn herdr-slackr");

    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.starts_with("slackr:"), "{err}");
    assert!(!err.contains("no config found"), "not a config error: {err}");
    assert!(!err.contains("tokens.toml"), "not a token error: {err}");
}

// ---- shared users cache -------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_secs()
}

/// A pre-seeded, fresh `users.json` in `state_dir` — enough for `users_cache::load` to hit
/// without ever touching the network.
fn seed_users_cache(state_dir: &Path) {
    std::fs::create_dir_all(state_dir).unwrap();
    let doc = serde_json::json!({
        "fetched_at": now_secs(),
        "users": [["U1", "Alice"]],
    });
    std::fs::write(state_dir.join("users.json"), doc.to_string()).unwrap();
}

/// The CLI reads a pre-seeded, fresh `users.json` from a fixture state dir (env-injected via
/// `HERDR_PLUGIN_STATE_DIR`, same pattern as `HERDR_PLUGIN_CONFIG_DIR` above) instead of
/// fetching `users.list` fresh. `cli::scan`'s error-priority contract puts `auth_self` first
/// among *network* calls, but the on-disk cache check itself is pure and happens ahead of that
/// (see `src/cli.rs`) — so a cache hit is logged, and the fake token still fails at `auth_self`
/// right after, without ever needing `users.list`. The state dir's `slackr.log` (enabled by the
/// same env var) records the hit regardless of that later REST failure.
#[test]
fn mentions_reads_a_pre_seeded_fresh_users_cache_instead_of_fetching() {
    let fixture = rest_fixture();
    let state_dir = tempfile::tempdir().unwrap();
    seed_users_cache(state_dir.path());

    let out = Command::new(bin())
        .args(["mentions"])
        .env("HERDR_PLUGIN_CONFIG_DIR", fixture.path())
        .env("HERDR_PLUGIN_STATE_DIR", state_dir.path())
        .env_remove("SLACK_APP_TOKEN")
        .env_remove("SLACK_USER_TOKEN")
        .output()
        .expect("spawn herdr-slackr");

    // Unaffected by the cache: auth_self still hits the network with the fake token and fails.
    assert_eq!(out.status.code(), Some(1));

    let log = std::fs::read_to_string(state_dir.path().join("slackr.log"))
        .expect("slackr.log written under HERDR_PLUGIN_STATE_DIR");
    assert!(log.contains("users_cache: hit"), "expected a cache-hit log line: {log}");
    assert!(log.contains("1 users"), "expected the pre-seeded user count: {log}");
}

// ---- usage / exit codes ---------------------------------------------------------------------

#[test]
fn mentions_limit_zero_is_a_usage_error() {
    let out = Command::new(bin()).args(["mentions", "--limit", "0"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("usage:"), "{}", stderr(&out));
}

#[test]
fn mentions_unknown_flag_is_a_usage_error() {
    let out = Command::new(bin()).args(["mentions", "--bogus"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("usage:"), "{}", stderr(&out));
}

#[test]
fn feed_unknown_flag_is_a_usage_error() {
    let out = Command::new(bin()).args(["feed", "--bogus"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("usage:"), "{}", stderr(&out));
}

// ---- skill-path ---------------------------------------------------------------------------

#[test]
fn skill_path_finds_the_dev_checkout_from_the_repo_root() {
    let out =
        Command::new(bin()).args(["skill-path"]).current_dir(manifest_dir()).output().unwrap();
    assert!(out.status.success(), "skill-path failed: {}", stderr(&out));
    let path = stdout(&out).trim().to_string();
    assert!(
        path.ends_with("skills/herdr-slackr/SKILL.md")
            || path.ends_with("skills\\herdr-slackr\\SKILL.md"),
        "path is the skill file: {path}"
    );
}

#[test]
fn skill_path_exits_1_naming_both_candidates_when_neither_exists() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin()).args(["skill-path"]).current_dir(dir.path()).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("SKILL.md"), "{}", stderr(&out));
}

// ---- skill-install --------------------------------------------------------------------------

fn skill_install(target: &Path, extra: &[&str]) -> Output {
    let target_str = target.display().to_string();
    let mut args = vec!["skill-install", "--target", target_str.as_str()];
    args.extend_from_slice(extra);
    Command::new(bin()).args(&args).current_dir(manifest_dir()).output().unwrap()
}

#[test]
fn skill_install_creates_the_file_with_installed_hint() {
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("SKILL.md");

    let out = skill_install(target.path(), &[]);
    assert!(out.status.success(), "skill-install failed: {}", stderr(&out));
    let out_stdout = stdout(&out);
    assert!(out_stdout.contains("installed:"), "{out_stdout}");
    assert!(out_stdout.contains("herdr-slackr mentions --json"), "{out_stdout}");

    #[cfg(unix)]
    {
        let meta = std::fs::symlink_metadata(&dest).expect("dest exists");
        assert!(meta.file_type().is_symlink(), "default install is a symlink");
    }
    #[cfg(windows)]
    {
        assert!(dest.exists(), "dest exists");
        let source = stdout(
            &Command::new(bin()).args(["skill-path"]).current_dir(manifest_dir()).output().unwrap(),
        )
        .trim()
        .to_string();
        assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&source).unwrap());
    }
}

#[test]
fn skill_install_twice_is_idempotent() {
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("SKILL.md");

    let first = skill_install(target.path(), &[]);
    assert!(first.status.success());
    let before = std::fs::symlink_metadata(&dest).unwrap().modified().ok();

    let second = skill_install(target.path(), &[]);
    assert!(second.status.success());
    assert!(stdout(&second).contains("already installed"), "{}", stdout(&second));
    let after = std::fs::symlink_metadata(&dest).unwrap().modified().ok();
    assert_eq!(before, after, "file unchanged by the second run");
}

#[test]
fn skill_install_refuses_to_clobber_a_conflicting_file_without_force() {
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("SKILL.md");
    std::fs::create_dir_all(target.path()).unwrap();
    std::fs::write(&dest, "not the skill").unwrap();

    let out = skill_install(target.path(), &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains(&dest.display().to_string()), "{}", stderr(&out));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "not the skill");

    let forced = skill_install(target.path(), &["--force"]);
    assert!(forced.status.success(), "--force replaces: {}", stderr(&forced));
    assert_ne!(std::fs::read_to_string(&dest).unwrap(), "not the skill");
}

#[test]
fn skill_install_copy_forces_a_regular_byte_identical_file() {
    let target = tempfile::tempdir().unwrap();
    let dest = target.path().join("SKILL.md");

    let out = skill_install(target.path(), &["--copy"]);
    assert!(out.status.success(), "skill-install --copy failed: {}", stderr(&out));

    let meta = std::fs::symlink_metadata(&dest).expect("dest exists");
    assert!(!meta.file_type().is_symlink(), "--copy installs a regular file");

    let source = stdout(
        &Command::new(bin()).args(["skill-path"]).current_dir(manifest_dir()).output().unwrap(),
    )
    .trim()
    .to_string();
    assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&source).unwrap());
}

#[test]
fn skill_install_with_an_unknown_flag_exits_2_with_usage_on_stderr() {
    let target = tempfile::tempdir().unwrap();
    let out = skill_install(target.path(), &["--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("usage:"), "{}", stderr(&out));
}

/// A self-contained dev checkout: a tempdir with `skills/herdr-slackr/SKILL.md` copied in, so
/// `resolve_skill_source`'s cwd-relative fallback finds it without touching the real repo.
/// Used as the cwd for `--project`, whose destination is also cwd-relative.
fn fake_checkout() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("skills/herdr-slackr");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::copy(manifest_dir().join("skills/herdr-slackr/SKILL.md"), skill_dir.join("SKILL.md"))
        .unwrap();
    dir
}

#[test]
fn project_flag_installs_into_dot_agents_skills_in_the_cwd() {
    let checkout = fake_checkout();
    let out = Command::new(bin())
        .args(["skill-install", "--project"])
        .current_dir(checkout.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "skill-install --project failed: {}", stderr(&out));

    let dest = checkout.path().join(".agents/skills/herdr-slackr/SKILL.md");
    assert!(dest.exists(), "installed at {}", dest.display());
    let out_stdout = stdout(&out);
    assert!(out_stdout.contains("installed:"), "{out_stdout}");
    assert!(
        out_stdout.replace('\\', "/").contains(".agents/skills/herdr-slackr/SKILL.md"),
        "{out_stdout}"
    );
}

#[test]
fn project_and_target_together_exit_2() {
    let checkout = fake_checkout();
    let target = tempfile::tempdir().unwrap();
    let target_str = target.path().display().to_string();
    let out = Command::new(bin())
        .args(["skill-install", "--project", "--target", target_str.as_str()])
        .current_dir(checkout.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("usage:"), "{}", stderr(&out));
}
