//! `herdr-slackr mentions|feed|skill-path|skill-install` — the agent-facing CLI.
//!
//! See `docs/superpowers/specs/2026-07-12-agent-cli-design.md`. Unlike `crate::run` (the
//! long-running pane), every subcommand here is a short-lived, read-only process: it opens a
//! fresh [`crate::rest::Rest`] session, makes the same Web API calls the pane's backfill makes
//! (`auth.test`, `conversations.list`, `conversations.history`, `users.list`), and prints. No
//! Slack write method is ever called from this module — that invariant holds for the whole
//! crate, but it is the whole *point* of this one: an agent gets read access to the feed
//! without gaining a way to post as the user.
//!
//! Arg parsing is a hand-rolled loop (closed dependency list, no `clap`): an unknown flag, a
//! flag missing its value, or `--limit 0` is a usage error (exit 2); a config, token, or REST
//! failure is exit 1 with one `slackr: …` stderr line; success is exit 0.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use crate::config::{self, PluginConfig};
use crate::entities::{self, is_mention};
use crate::model::{ConvKind, Conversation, Message, resolve_channels, ts_cmp, ts_key};
use crate::rest::{self, Rest, RestError};
use crate::tokens;

const USAGE: &str = "usage: herdr-slackr mentions [--json] [--limit <n>]\n       herdr-slackr feed [--channel \"#name\"] [--json] [--limit <n>]\n       herdr-slackr skill-path\n       herdr-slackr skill-install [--target <dir> | --project] [--copy] [--force]\n";

/// The pane's backfill depth per conversation (spec: "the pane's backfill depth,
/// 50/conversation"), reused here so a fresh CLI invocation sees the same window.
const HISTORY_LIMIT: u32 = 50;

/// The default row cap for `mentions`/`feed` when `--limit` is not given.
const DEFAULT_LIMIT: u32 = 20;

/// Entry point called from `main` with the full process argv (`args[0]` is the program name,
/// `args[1]` the subcommand). Only reached when `main` has already confirmed `args[1]` is one
/// of the four subcommands this module owns.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Vec<String>) -> ExitCode {
    crate::log::init();
    match args.get(1).map(String::as_str) {
        Some("mentions") => mentions(&args[2..]),
        Some("feed") => feed(&args[2..]),
        Some("skill-path") => skill_path(),
        Some("skill-install") => skill_install(&args[2..]),
        _ => usage_error(),
    }
}

/// Whether `args[1]` names one of this module's subcommands — `main` uses this to decide
/// whether to dispatch here before the pane's own guard/launch path.
#[must_use]
pub fn owns(args: &[String]) -> bool {
    matches!(
        args.get(1).map(String::as_str),
        Some("mentions" | "feed" | "skill-path" | "skill-install")
    )
}

fn usage_error() -> ExitCode {
    eprint!("{USAGE}");
    ExitCode::from(2)
}

// ---- config/token discovery -------------------------------------------------------------------

/// Resolve the plugin config directory: `HERDR_PLUGIN_CONFIG_DIR` (via `env_fn`) when set, else
/// `<home>/.config/herdr/plugins/config/dcieslak19973.slackr` using `home_fn`'s
/// `${HOME:-USERPROFILE}` lookup. Neither source available is itself an error naming both
/// candidates, so callers get one place to report from. Pure and closure-injected so it is
/// testable without touching real process environment state.
fn config_dir(
    env_fn: impl Fn(&str) -> Option<String>,
    home_fn: impl Fn() -> Option<String>,
) -> Result<PathBuf, String> {
    if let Some(dir) = env_fn("HERDR_PLUGIN_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    match home_fn() {
        Some(home) => Ok(fallback_dir(&home)),
        None => Err("no config found: set HERDR_PLUGIN_CONFIG_DIR, or set HOME/USERPROFILE so \
             ~/.config/herdr/plugins/config/dcieslak19973.slackr can be located"
            .to_string()),
    }
}

/// `<home>/.config/herdr/plugins/config/dcieslak19973.slackr`, herdr's standard plugin config
/// layout (matches the pane's own `HERDR_PLUGIN_CONFIG_DIR` when herdr sets it).
fn fallback_dir(home: &str) -> PathBuf {
    Path::new(home).join(".config/herdr/plugins/config/dcieslak19973.slackr")
}

fn real_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn real_home() -> Option<String> {
    std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok())
}

/// Load the plugin config, naming both candidate locations in the error when neither yields a
/// readable `config.toml` — the env var (its value, or "not set") and the computed
/// `~/.config/...` fallback (or "HOME/USERPROFILE not set" when that can't be computed either).
fn load_config() -> Result<(PathBuf, PluginConfig), String> {
    let dir = config_dir(real_env, real_home)?;
    if let Ok(cfg) = config::plugin_config_in(&dir) {
        return Ok((dir, cfg));
    }
    let env_desc = real_env("HERDR_PLUGIN_CONFIG_DIR").map_or_else(
        || "HERDR_PLUGIN_CONFIG_DIR (not set)".to_string(),
        |v| format!("HERDR_PLUGIN_CONFIG_DIR={v}"),
    );
    let fallback_desc = real_home().map_or_else(
        || {
            "~/.config/herdr/plugins/config/dcieslak19973.slackr (HOME/USERPROFILE not set)"
                .to_string()
        },
        |home| fallback_dir(&home).display().to_string(),
    );
    Err(format!("no config found; tried {env_desc} and {fallback_desc}"))
}

// ---- mentions/feed orchestration ---------------------------------------------------------------

fn mentions(args: &[String]) -> ExitCode {
    let mut json = false;
    let mut limit = DEFAULT_LIMIT;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--json" => json = true,
            "--limit" => {
                let Some(n) = parse_limit(it.next()) else { return usage_error() };
                limit = n;
            }
            _ => return usage_error(),
        }
    }
    scan(json, limit, None::<&str>, true)
}

fn feed(args: &[String]) -> ExitCode {
    let mut json = false;
    let mut limit = DEFAULT_LIMIT;
    let mut channel: Option<String> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--json" => json = true,
            "--limit" => {
                let Some(n) = parse_limit(it.next()) else { return usage_error() };
                limit = n;
            }
            "--channel" => {
                let Some(value) = it.next() else { return usage_error() };
                channel = Some(value.clone());
            }
            _ => return usage_error(),
        }
    }
    scan(json, limit, channel.as_deref(), false)
}

/// `--limit`'s value: a positive integer, `None` for anything else (missing value, not a
/// number, or zero — "limit must be >= 1").
fn parse_limit(raw: Option<&String>) -> Option<u32> {
    let n: u32 = raw?.parse().ok()?;
    if n == 0 { None } else { Some(n) }
}

/// The shared scan behind `mentions` and `feed`: resolve config/tokens, fetch conversations and
/// history fresh, filter (by mention or by `--channel`), sort newest-first, cap, and print.
fn scan(json: bool, limit: u32, channel_filter: Option<&str>, mention_only: bool) -> ExitCode {
    let (dir, config) = match load_config() {
        Ok(v) => v,
        Err(message) => return fail(&message),
    };
    let tokens = match tokens::resolve(&dir, |name| std::env::var(name).ok()) {
        Ok(t) => t,
        Err(error) => return fail(&error.0),
    };

    let cancelled = AtomicBool::new(false);
    let rest = Rest { user_token: &tokens.user, cancelled: &cancelled };

    // Error-priority contract: `auth_self` is the first *network* call on every scan — a cheap
    // identity check, so a bad/expired token surfaces as a clean `invalid_auth` before any other
    // call has a chance to fail first with a less specific error. The users cache's on-disk read
    // (`cached_users`) is checked before that, since it never touches the network and so can't
    // violate the ordering — a fresh cache answers `users.list` without ever needing `auth_self`
    // to have run. Only on a cache miss does fetching `users.list` (`users_cached`'s network
    // fallback) happen, and only after `auth_self` has already succeeded. `list_conversations`
    // runs last.
    let now = crate::users_cache::now_secs();
    let state_dir = crate::users_cache::state_dir(real_env, real_home);
    let cached_users = crate::users_cache::cached_users(state_dir.as_deref(), now);

    let self_id = match rest::auth_self(&rest) {
        Ok(id) => id,
        Err(error) => return rest_fail(&error),
    };

    let users = match cached_users {
        Some(v) => v,
        None => match crate::users_cache::users_cached(&rest, state_dir.as_deref(), now) {
            Ok(v) => v,
            Err(error) => return rest_fail(&error),
        },
    };
    let user_names: HashMap<String, String> = users.into_iter().collect();

    let all_convs = match rest::list_conversations(&rest) {
        Ok(v) => v,
        Err(error) => return rest_fail(&error),
    };

    let selected = match resolve_channels(
        config.channels(),
        config.dms(),
        config.dm_limit(),
        config.dm_allow(),
        &all_convs,
    ) {
        Ok(v) => v,
        Err(message) => return fail(&message),
    };
    let selected = resolve_im_names(selected, &user_names);

    let selected = if let Some(wanted) = channel_filter {
        let name = wanted.strip_prefix('#').unwrap_or(wanted);
        if let Some(c) = selected.iter().find(|c| c.name == name) {
            vec![c.clone()]
        } else {
            let configured = config.channels().join(", ");
            return fail(&format!("unknown channel: {wanted} (configured: {configured})"));
        }
    } else {
        selected
    };

    let conv_names: HashMap<String, String> =
        selected.iter().map(|c| (c.id.clone(), c.name.clone())).collect();
    let conv_kinds: HashMap<String, ConvKind> =
        selected.iter().map(|c| (c.id.clone(), c.kind)).collect();

    let mut msgs = Vec::new();
    let total = selected.len();
    let mut partial_note = None;
    for (scanned, conv) in selected.iter().enumerate() {
        match rest::history(&rest, &conv.id, HISTORY_LIMIT, None) {
            Ok(v) => msgs.extend(v),
            Err(error) => match scan_outcome(scanned, total, &error) {
                ScanOutcome::HardFail => return rest_fail(&error),
                ScanOutcome::Partial(note) => {
                    partial_note = Some(note);
                    break;
                }
            },
        }
    }

    let mut rows = if mention_only {
        select_mentions(msgs, &conv_kinds, &self_id, config.keywords())
    } else {
        msgs.sort_by(|a, b| ts_cmp(&b.ts, &a.ts));
        msgs
    };
    rows.truncate(limit as usize);

    if json {
        print_json(&rows, &conv_names, &conv_kinds, &user_names);
    } else {
        print_human(&rows, &conv_names, &conv_kinds, &user_names);
    }
    if let Some(note) = partial_note {
        eprintln!("{note}");
    }
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("slackr: {message}");
    ExitCode::from(1)
}

/// `RestError` → exit 1: a rate limit gets the fixed remedy line the spec pins, everything
/// else prints the classified detail (Slack's own error name for `SlackError`).
fn rest_fail(error: &RestError) -> ExitCode {
    eprintln!("slackr: {}", rest_error_summary(error));
    ExitCode::from(1)
}

/// The one-line classification of a `RestError` shared by `rest_fail` (hard failure) and
/// `scan_outcome` (partial-results note): the rate-limit remedy line the spec pins, or Slack's
/// own error name/detail otherwise.
fn rest_error_summary(error: &RestError) -> String {
    match error {
        RestError::RateLimited(secs) => format!("slack rate limit — retry in {secs}s"),
        RestError::SlackError(name) => name.clone(),
        RestError::NoCurl => "curl not found on PATH".to_string(),
        RestError::Other(detail) => detail.clone(),
    }
}

/// The scan loop's decision on a mid-scan `history()` failure (maintainer decision: partial
/// results beat discarding work already fetched, mirroring `app.rs::poll_tick`'s per-tick
/// graceful degradation). `scanned` conversations already succeeded before this failure, out of
/// `total` selected: zero scanned means the very first conversation failed, so there is nothing
/// to show — a hard failure, same as the pre-fix behavior. Otherwise the loop stops here but
/// keeps what it has: the caller prints the rows already collected as normal, then this one
/// stderr note. Pure — no I/O — so the branch and wording are unit-tested without a REST call.
enum ScanOutcome {
    /// Print rows normally, then this one `slackr: partial results — …` stderr line; exit 0.
    Partial(String),
    /// Discard everything (there is nothing to keep); `rest_fail`'s line, no stdout, exit 1.
    HardFail,
}

fn scan_outcome(scanned: usize, total: usize, error: &RestError) -> ScanOutcome {
    if scanned == 0 {
        ScanOutcome::HardFail
    } else {
        ScanOutcome::Partial(format!(
            "slackr: partial results — {} after {scanned}/{total} conversations",
            rest_error_summary(error)
        ))
    }
}

/// Filter `msgs` to the ones that fire [`is_mention`] (using `kinds` to look up each message's
/// conversation kind, defaulting to `Channel` for a conversation this scan didn't select), then
/// sort newest-first by [`ts_cmp`]. Pure — no I/O, no time source — so it is unit-tested
/// directly on fixture messages.
fn select_mentions(
    msgs: Vec<Message>,
    kinds: &HashMap<String, ConvKind>,
    self_id: &str,
    keywords: &[String],
) -> Vec<Message> {
    let mut out: Vec<Message> = msgs
        .into_iter()
        .filter(|m| {
            let kind = kinds.get(&m.conv).copied().unwrap_or(ConvKind::Channel);
            is_mention(m, kind, self_id, keywords)
        })
        .collect();
    out.sort_by(|a, b| ts_cmp(&b.ts, &a.ts));
    out
}

/// One human-readable row: `#chan  @author  HH:MM  text` (or `@author` prefixed with `@` for a
/// DM's conversation label, matching the pane's `conv_label`). Pure and unit-tested.
fn format_row(conv_label: &str, author: &str, ts: &str, text: &str) -> String {
    format!("{conv_label}  @{author}  {}  {text}", ts_to_hhmm(ts))
}

/// `#chan` or `@name` depending on conversation kind — the same convention `crate::app`'s
/// `conv_label` uses for the pane's rows.
fn conv_label(
    id: &str,
    names: &HashMap<String, String>,
    kinds: &HashMap<String, ConvKind>,
) -> String {
    let kind = kinds.get(id).copied().unwrap_or(ConvKind::Channel);
    let name = names.get(id).cloned().unwrap_or_else(|| id.to_string());
    match kind {
        ConvKind::Im => format!("@{name}"),
        ConvKind::Channel | ConvKind::Group | ConvKind::Mpim => format!("#{name}"),
    }
}

fn print_human(
    rows: &[Message],
    names: &HashMap<String, String>,
    kinds: &HashMap<String, ConvKind>,
    users: &HashMap<String, String>,
) {
    for msg in rows {
        let label = conv_label(&msg.conv, names, kinds);
        let author = users.get(&msg.author).cloned().unwrap_or_else(|| msg.author.clone());
        let text =
            entities::resolve(&msg.text, |id| users.get(id).cloned(), |id| names.get(id).cloned());
        println!("{}", format_row(&label, &author, &msg.ts, &text));
    }
}

fn print_json(
    rows: &[Message],
    names: &HashMap<String, String>,
    kinds: &HashMap<String, ConvKind>,
    users: &HashMap<String, String>,
) {
    let docs: Vec<serde_json::Value> = rows
        .iter()
        .map(|msg| {
            let label = conv_label(&msg.conv, names, kinds);
            let author = users.get(&msg.author).cloned().unwrap_or_else(|| msg.author.clone());
            let text = entities::resolve(
                &msg.text,
                |id| users.get(id).cloned(),
                |id| names.get(id).cloned(),
            );
            serde_json::json!({
                "conversation": label,
                "conv_id": msg.conv,
                "author": author,
                "author_id": msg.author,
                "ts": msg.ts,
                "text": text,
                "text_raw": msg.text,
            })
        })
        .collect();
    println!("{}", serde_json::Value::Array(docs));
}

/// Render a Slack `ts`'s seconds as a `HH:MM` UTC clock time. Duplicated from
/// `crate::app`'s private `ts_to_hhmm` (not `pub`, so it can't be reused directly) — kept
/// identical in behavior; a malformed `ts` renders `00:00` rather than panicking, via
/// `ts_key`'s `(0, 0)` fallback.
fn ts_to_hhmm(ts: &str) -> String {
    let (secs, _) = ts_key(ts);
    let day_secs = secs % 86_400;
    format!("{:02}:{:02}", day_secs / 3600, (day_secs % 3600) / 60)
}

/// Resolve each `Im` conversation's display name via the `users.list` cache, falling back to
/// the raw id when unresolved. Duplicated from `crate::app`'s private `resolve_im_names`.
fn resolve_im_names(
    convs: Vec<Conversation>,
    users: &HashMap<String, String>,
) -> Vec<Conversation> {
    convs
        .into_iter()
        .map(|c| {
            if c.kind == ConvKind::Im {
                let name = users.get(&c.name).cloned().unwrap_or_else(|| c.name.clone());
                Conversation { name, ..c }
            } else {
                c
            }
        })
        .collect()
}

// ---- skill-path / skill-install -----------------------------------------------------------------

/// `<plugin-root>/skills/herdr-slackr/SKILL.md`, where `plugin-root` is the running
/// executable's directory's parent (`bin/..`). Falls back to the cwd-relative dev-checkout path
/// when the installed layout isn't found (running `cargo run`/`cargo test` from a checkout
/// rather than the packaged plugin). Ported from `herdr-reviewr`'s `resolve_skill_source`,
/// adapted to this crate's skill directory.
fn resolve_skill_source() -> Result<PathBuf, String> {
    const REL: &str = "skills/herdr-slackr/SKILL.md";
    let installed = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|bin| bin.parent().map(Path::to_path_buf))
        .map(|plugin_root| plugin_root.join(REL));
    if let Some(path) = &installed
        && path.exists()
    {
        return Ok(path.clone());
    }

    let dev_checkout = PathBuf::from(REL);
    if dev_checkout.exists() {
        return Ok(dev_checkout);
    }

    let installed_display =
        installed.as_ref().map_or_else(|| REL.to_string(), |p| p.display().to_string());
    Err(format!("SKILL.md not found at {installed_display} or {}", dev_checkout.display()))
}

fn skill_path() -> ExitCode {
    match resolve_skill_source() {
        Ok(path) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Err(message) => fail(&message),
    }
}

/// `$HOME/.claude/skills/herdr-slackr` (`%USERPROFILE%` on Windows), the default
/// `skill-install` target when `--target` isn't given. `None` when neither environment
/// variable is set.
fn default_skill_install_target() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| Path::new(&home).join(".claude").join("skills").join("herdr-slackr"))
}

/// The stdout hint printed after a fresh install (or a `--force` replace), reminding the user
/// that a proactive agent needs the reminder in `CLAUDE.md` too — the skill list alone only
/// covers the "user asks for it" path.
fn print_installed(dest: &Path) {
    println!("installed: {}", dest.display());
    println!("To make agents check Slack proactively, add to your CLAUDE.md:");
    println!("  Slack triage happens via herdr-slackr — when the user asks about mentions or a");
    println!("  channel, run `herdr-slackr mentions --json` / `herdr-slackr feed --channel …`.");
}

/// True when `a` and `b` refer to the same file, comparing canonicalized paths where possible
/// and falling back to a literal comparison (e.g. a dangling symlink target) otherwise.
fn paths_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Installs the bundled `SKILL.md` (resolved exactly as `skill-path` does) into `dest`,
/// symlinking on Unix and falling back to a copy — with a one-line stderr note — on Windows or
/// when symlink creation fails for any reason (typically privileges).
fn install_skill(source: &Path, dest: &Path, copy: bool) -> ExitCode {
    if !copy {
        #[cfg(unix)]
        {
            if std::os::unix::fs::symlink(source, dest).is_ok() {
                print_installed(dest);
                return ExitCode::SUCCESS;
            }
        }
        eprintln!("slackr: symlink unavailable — copied; re-run after plugin updates");
    }
    match std::fs::copy(source, dest) {
        Ok(_) => {
            print_installed(dest);
            ExitCode::SUCCESS
        }
        Err(error) => fail(&format!("cannot install to {}: {error}", dest.display())),
    }
}

/// Installs the bundled skill into a Claude Code (or compatible) skills directory so mention
/// triage works with no per-session reminder. See the module doc and `SKILL.md` for the full
/// contract. Ported from `herdr-reviewr`'s `skill_install`.
fn skill_install(args: &[String]) -> ExitCode {
    let mut target: Option<PathBuf> = None;
    let mut project = false;
    let mut copy = false;
    let mut force = false;

    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--target" => {
                let Some(value) = it.next() else { return usage_error() };
                target = Some(PathBuf::from(value));
            }
            "--project" => project = true,
            "--copy" => copy = true,
            "--force" => force = true,
            _ => return usage_error(),
        }
    }

    if project && target.is_some() {
        return usage_error();
    }
    if project {
        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(error) => return fail(&format!("cannot read current directory: {error}")),
        };
        target = Some(cwd.join(".agents").join("skills").join("herdr-slackr"));
    }

    let Some(target_dir) = target.or_else(default_skill_install_target) else {
        return fail("cannot determine home directory (set HOME or USERPROFILE, or pass --target)");
    };

    let source = match resolve_skill_source() {
        Ok(path) => path,
        Err(message) => return fail(&message),
    };
    let canonical_source = std::fs::canonicalize(&source).unwrap_or(source);

    if let Err(error) = std::fs::create_dir_all(&target_dir) {
        return fail(&format!("cannot create {}: {error}", target_dir.display()));
    }
    let dest = target_dir.join("SKILL.md");

    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        let already_installed = if meta.file_type().is_symlink() {
            std::fs::read_link(&dest).is_ok_and(|link| {
                let resolved = if link.is_absolute() { link } else { target_dir.join(link) };
                paths_equal(&resolved, &canonical_source)
            })
        } else {
            matches!(
                (std::fs::read(&dest), std::fs::read(&canonical_source)),
                (Ok(existing), Ok(wanted)) if existing == wanted
            )
        };
        if already_installed {
            println!("already installed at {}", dest.display());
            return ExitCode::SUCCESS;
        }
        if !force {
            return fail(&format!(
                "{} already exists and differs from the bundled skill; re-run with --force to replace",
                dest.display()
            ));
        }
        if let Err(error) = std::fs::remove_file(&dest) {
            return fail(&format!("cannot remove existing {}: {error}", dest.display()));
        }
    }

    install_skill(&canonical_source, &dest, copy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(conv: &str, ts: &str, author: &str, text: &str) -> Message {
        Message {
            conv: conv.to_string(),
            ts: ts.to_string(),
            thread_ts: None,
            author: author.to_string(),
            text: text.to_string(),
            edited: false,
            reply_count: None,
            reactions: Vec::new(),
        }
    }

    fn conv(id: &str, name: &str, kind: ConvKind) -> Conversation {
        Conversation { id: id.into(), name: name.into(), kind, updated: None }
    }

    // ---- config_dir --------------------------------------------------------------------------

    #[test]
    fn config_dir_env_wins_over_home_fallback() {
        let dir = config_dir(
            |name| (name == "HERDR_PLUGIN_CONFIG_DIR").then(|| "/env/dir".to_string()),
            || Some("/home/dan".to_string()),
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/env/dir"));
    }

    #[test]
    fn config_dir_falls_back_to_home_when_env_unset() {
        let dir = config_dir(|_| None, || Some("/home/dan".to_string())).unwrap();
        assert_eq!(
            dir,
            Path::new("/home/dan").join(".config/herdr/plugins/config/dcieslak19973.slackr")
        );
    }

    #[test]
    fn config_dir_errors_naming_both_when_neither_source_is_available() {
        let error = config_dir(|_| None, || None).unwrap_err();
        assert!(error.contains("HERDR_PLUGIN_CONFIG_DIR"), "{error}");
        assert!(error.contains("HOME"), "{error}");
    }

    // ---- select_mentions ----------------------------------------------------------------------

    #[test]
    fn select_mentions_filters_to_mention_hits_and_sorts_newest_first() {
        let kinds = HashMap::from([("C1".to_string(), ConvKind::Channel)]);
        let msgs = vec![
            msg("C1", "1.000001", "U1", "no mention here"),
            msg("C1", "3.000001", "U1", "<@SELF> urgent one"),
            msg("C1", "2.000001", "U1", "<@SELF> older mention"),
        ];
        let selected = select_mentions(msgs, &kinds, "SELF", &[]);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].ts, "3.000001", "newest mention first");
        assert_eq!(selected[1].ts, "2.000001");
    }

    #[test]
    fn select_mentions_includes_all_im_and_mpim_regardless_of_content() {
        let kinds = HashMap::from([("D1".to_string(), ConvKind::Im)]);
        let msgs = vec![msg("D1", "1.000001", "U1", "just chatting")];
        let selected = select_mentions(msgs, &kinds, "SELF", &[]);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn select_mentions_defaults_unknown_conv_to_channel_kind() {
        let kinds = HashMap::new();
        let msgs = vec![msg("C-unknown", "1.000001", "U1", "no mention here")];
        let selected = select_mentions(msgs, &kinds, "SELF", &[]);
        assert!(selected.is_empty());
    }

    // ---- format_row ---------------------------------------------------------------------------

    #[test]
    fn format_row_matches_the_spec_shape() {
        let row = format_row("#eng", "dan", "1752300000.000100", "hello");
        assert_eq!(row, "#eng  @dan  06:00  hello");
    }

    #[test]
    fn format_row_uses_the_at_prefixed_label_for_dms_unchanged() {
        let row = format_row("@dan", "dan", "1752300000.000100", "hi");
        assert_eq!(row, "@dan  @dan  06:00  hi");
    }

    // ---- resolve_channels: see `crate::model`'s tests (moved there — dedup, task 1) --------

    #[test]
    fn resolve_im_names_maps_the_counterpart_user_id_to_a_display_name() {
        let convs = vec![conv("D1", "U9", ConvKind::Im)];
        let users = HashMap::from([("U9".to_string(), "priya".to_string())]);
        let resolved = resolve_im_names(convs, &users);
        assert_eq!(resolved[0].name, "priya");
    }

    // ---- parse_limit ----------------------------------------------------------------------------

    #[test]
    fn parse_limit_rejects_zero() {
        assert_eq!(parse_limit(Some(&"0".to_string())), None);
    }

    #[test]
    fn parse_limit_accepts_a_positive_integer() {
        assert_eq!(parse_limit(Some(&"5".to_string())), Some(5));
    }

    #[test]
    fn parse_limit_rejects_non_numeric() {
        assert_eq!(parse_limit(Some(&"abc".to_string())), None);
    }

    // ---- ts_to_hhmm -----------------------------------------------------------------------------

    #[test]
    fn ts_to_hhmm_formats_epoch_seconds() {
        assert_eq!(ts_to_hhmm("1752300000.000100"), "06:00");
    }

    #[test]
    fn ts_to_hhmm_malformed_input_renders_midnight() {
        assert_eq!(ts_to_hhmm("garbage"), "00:00");
    }

    // ---- scan_outcome (partial-results decision) ---------------------------------------------

    #[test]
    fn scan_outcome_is_partial_when_at_least_one_conversation_already_scanned() {
        let outcome = scan_outcome(2, 5, &RestError::SlackError("channel_not_found".to_string()));
        match outcome {
            ScanOutcome::Partial(note) => {
                assert_eq!(
                    note,
                    "slackr: partial results — channel_not_found after 2/5 conversations"
                );
            }
            ScanOutcome::HardFail => panic!("expected Partial, got HardFail"),
        }
    }

    #[test]
    fn scan_outcome_is_partial_with_rate_limit_wording() {
        let outcome = scan_outcome(1, 3, &RestError::RateLimited(30));
        match outcome {
            ScanOutcome::Partial(note) => {
                assert_eq!(
                    note,
                    "slackr: partial results — slack rate limit — retry in 30s after 1/3 conversations"
                );
            }
            ScanOutcome::HardFail => panic!("expected Partial, got HardFail"),
        }
    }

    #[test]
    fn scan_outcome_is_hard_fail_when_the_first_conversation_fails() {
        let outcome = scan_outcome(0, 5, &RestError::SlackError("channel_not_found".to_string()));
        assert!(matches!(outcome, ScanOutcome::HardFail));
    }

    // ---- owns -----------------------------------------------------------------------------------

    #[test]
    fn owns_recognizes_all_four_subcommands() {
        for name in ["mentions", "feed", "skill-path", "skill-install"] {
            assert!(owns(&["herdr-slackr".to_string(), name.to_string()]));
        }
    }

    #[test]
    fn owns_is_false_for_anything_else() {
        assert!(!owns(&["herdr-slackr".to_string(), "bogus".to_string()]));
        assert!(!owns(&["herdr-slackr".to_string()]));
    }
}
