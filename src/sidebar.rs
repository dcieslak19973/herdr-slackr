//! `herdr-slackr sidebar <toggle|open|close>` — the feed-pane actions, in-process.
//!
//! Port of the retired `herdr/sidebar.sh` (see
//! `docs/superpowers/specs/2026-07-30-windows-support-design.md` §3): native Windows has no
//! `bash`/`jq`, and once the logic lives in the binary it is identical on every platform. The
//! contract is unchanged — one feed pane per workspace (any pane labeled `slack`, any tab),
//! fixed `split`/`right` placement, focus follows a manual open, refusals are one `slackr: …`
//! stderr line + exit 1, successes one stdout line. One deliberate tightening: a pane list
//! that fails to *parse* refuses like a pane list that failed to *run* — a bad listing must
//! not read as "no feed pane" (that would stack a duplicate on toggle and false-succeed a
//! close). Unknown or missing modes are usage errors (exit 2), like every other subcommand.
//!
//! The open names this platform's own pane entrypoint (`feed` on unix, `feed-windows` on
//! Windows): herdr rejects duplicate pane ids even across disjoint platform filters, so the
//! manifest carries one pane entry per platform (design §4).

use std::process::{Command, ExitCode, Stdio};

/// One feed-pane action, named by the manifest's action commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Toggle,
    Open,
    Close,
}

/// Parse the argv tail after `sidebar` — exactly one recognized mode, nothing else.
fn parse_mode(args: &[String]) -> Option<Mode> {
    match args {
        [mode] => match mode.as_str() {
            "toggle" => Some(Mode::Toggle),
            "open" => Some(Mode::Open),
            "close" => Some(Mode::Close),
            _ => None,
        },
        _ => None,
    }
}

/// The manifest pane entrypoint for this build's platform (design §4: duplicate pane ids are
/// rejected, so Windows has its own `feed-windows` twin).
const fn entrypoint() -> &'static str {
    if cfg!(windows) { "feed-windows" } else { "feed" }
}

/// The label the manifest's `[[panes]] title` gives every feed pane — both platform twins use
/// the same title, so label-based discovery is platform-agnostic.
const FEED_LABEL: &str = "slack";

/// `.focused_pane_cwd // .workspace_cwd // empty` from `HERDR_PLUGIN_CONTEXT_JSON`.
fn context_cwd(context_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(context_json).ok()?;
    ["focused_pane_cwd", "workspace_cwd"]
        .iter()
        .find_map(|k| v.get(k).and_then(serde_json::Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Every feed pane in a `pane list` response: `.result.panes[] | select(.label == "slack") |
/// .pane_id`. `None` when the response does not parse to that shape — the caller refuses
/// rather than treating it as "nothing open".
fn feed_panes(panes_json: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(panes_json).ok()?;
    let panes = v.get("result")?.get("panes")?.as_array()?;
    Some(
        panes
            .iter()
            .filter(|p| p.get("label").and_then(serde_json::Value::as_str) == Some(FEED_LABEL))
            .filter_map(|p| p.get("pane_id").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect(),
    )
}

/// The workspace's first pane — the attach target when the action ran without a focused pane.
fn first_pane(panes_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(panes_json).ok()?;
    v.get("result")?.get("panes")?.as_array()?.first()?.get("pane_id")?.as_str().map(str::to_string)
}

/// The herdr argv (after the binary name) that opens the feed pane: fixed split/right, focus
/// follows the open, `--cwd` only when the invocation context supplied one.
fn open_argv(plugin_id: &str, target_pane: &str, cwd: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = [
        "plugin",
        "pane",
        "open",
        "--plugin",
        plugin_id,
        "--entrypoint",
        entrypoint(),
        "--placement",
        "split",
        "--target-pane",
        target_pane,
        "--direction",
        "right",
        "--focus",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    if let Some(cwd) = cwd {
        argv.push("--cwd".to_string());
        argv.push(cwd.to_string());
    }
    argv
}

/// `.result.plugin_pane.pane.pane_id` from a `plugin pane open` response.
fn opened_pane_id(open_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(open_json).ok()?;
    v.get("result")?
        .get("plugin_pane")?
        .get("pane")?
        .get("pane_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ---- the thin runner over real env + herdr ----------------------------------------------------

fn refuse(msg: &str) -> ExitCode {
    eprintln!("slackr: {msg}");
    ExitCode::FAILURE
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// Run herdr with `args`, returning stdout only on a zero exit. Stderr is discarded, matching
/// the script's `2>/dev/null` — herdr's own diagnostics never leak into the action log lines.
fn herdr_out(herdr: &str, args: &[String]) -> Option<String> {
    let out =
        Command::new(herdr).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Close every listed feed pane via plain `pane close` (not `plugin pane close`: the
/// plugin-pane registry does not survive a herdr restart and would strand the pane).
fn close_all(herdr: &str, workspace: &str, existing: &[String]) -> ExitCode {
    let mut failed: Vec<&str> = Vec::new();
    for pane in existing {
        let args = ["pane".to_string(), "close".to_string(), pane.clone()];
        if herdr_out(herdr, &args).is_none() {
            failed.push(pane);
        }
    }
    if failed.is_empty() {
        println!("closed {} in {workspace}", existing.join(" "));
        ExitCode::SUCCESS
    } else {
        refuse(&format!("failed to close {} in {workspace}", failed.join(" ")))
    }
}

/// Entry point from `cli::run` with the argv tail after `sidebar`.
pub fn run(args: &[String]) -> ExitCode {
    let Some(mode) = parse_mode(args) else {
        return crate::cli::usage_error();
    };
    let Some(workspace) = env_nonempty("HERDR_WORKSPACE_ID") else {
        return refuse("no workspace context (invoke from inside herdr)");
    };
    let herdr = env_nonempty("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".to_string());

    // One pane-list snapshot serves the whole run; a failed or unparseable listing refuses.
    let list_args: Vec<String> = ["pane", "list", "--workspace", workspace.as_str()]
        .iter()
        .map(ToString::to_string)
        .collect();
    let Some(panes_json) = herdr_out(&herdr, &list_args) else {
        return refuse(&format!("herdr pane list failed for {workspace}"));
    };
    let Some(existing) = feed_panes(&panes_json) else {
        return refuse(&format!("herdr pane list failed for {workspace}"));
    };

    match mode {
        Mode::Close => {
            if existing.is_empty() {
                println!("close: nothing open in {workspace}");
                ExitCode::SUCCESS
            } else {
                close_all(&herdr, &workspace, &existing)
            }
        }
        Mode::Toggle if !existing.is_empty() => close_all(&herdr, &workspace, &existing),
        Mode::Open if !existing.is_empty() => {
            println!("open: already open ({}) in {workspace}", existing.join(" "));
            ExitCode::SUCCESS
        }
        Mode::Toggle | Mode::Open => {
            let Some(target) = env_nonempty("HERDR_PANE_ID").or_else(|| first_pane(&panes_json))
            else {
                return refuse(&format!("no pane to attach to in {workspace}"));
            };
            let plugin_id = env_nonempty("HERDR_PLUGIN_ID")
                .unwrap_or_else(|| "dcieslak19973.slackr".to_string());
            let cwd = env_nonempty("HERDR_PLUGIN_CONTEXT_JSON").and_then(|j| context_cwd(&j));
            let open_args = open_argv(&plugin_id, &target, cwd.as_deref());
            let Some(new) = herdr_out(&herdr, &open_args).as_deref().and_then(opened_pane_id)
            else {
                return refuse("herdr plugin pane open failed");
            };
            println!("opened {new} (split) in {workspace}");
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, context_cwd, feed_panes, first_pane, open_argv, opened_pane_id, parse_mode};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parse_mode_accepts_exactly_one_known_mode() {
        assert_eq!(parse_mode(&s(&["toggle"])), Some(Mode::Toggle));
        assert_eq!(parse_mode(&s(&["open"])), Some(Mode::Open));
        assert_eq!(parse_mode(&s(&["close"])), Some(Mode::Close));
        assert_eq!(parse_mode(&s(&[])), None, "missing mode");
        assert_eq!(parse_mode(&s(&["auto-open"])), None, "slackr has no event hook");
        assert_eq!(parse_mode(&s(&["toggle", "open"])), None, "extra args");
    }

    #[test]
    fn context_cwd_prefers_focused_pane_over_workspace() {
        let both = r#"{"focused_pane_cwd": "/a", "workspace_cwd": "/b"}"#;
        assert_eq!(context_cwd(both).as_deref(), Some("/a"));
        let ws_only = r#"{"workspace_cwd": "/b"}"#;
        assert_eq!(context_cwd(ws_only).as_deref(), Some("/b"));
        assert_eq!(context_cwd(r#"{"focused_pane_cwd": ""}"#), None, "empty is absent");
        assert_eq!(context_cwd("not json"), None);
    }

    #[test]
    fn feed_panes_selects_slack_labeled_panes_only() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","label":"agent"},
            {"pane_id":"w1:p2","label":"slack"},
            {"pane_id":"w1:p3"},
            {"pane_id":"w1:p4","label":"slack"}
        ]}}"#;
        assert_eq!(feed_panes(json), Some(vec!["w1:p2".to_string(), "w1:p4".to_string()]));
    }

    #[test]
    fn feed_panes_refuses_rather_than_reading_garbage_as_empty() {
        assert_eq!(feed_panes("not json"), None);
        assert_eq!(feed_panes(r#"{"result":{}}"#), None);
        assert_eq!(feed_panes(r#"{"result":{"panes":[]}}"#), Some(vec![]), "a real empty list");
    }

    #[test]
    fn first_pane_is_the_attach_fallback() {
        let json = r#"{"result":{"panes":[{"pane_id":"w1:p9","label":"agent"}]}}"#;
        assert_eq!(first_pane(json).as_deref(), Some("w1:p9"));
        assert_eq!(first_pane(r#"{"result":{"panes":[]}}"#), None);
    }

    #[test]
    fn open_argv_is_the_fixed_split_right_focused_open() {
        assert_eq!(
            open_argv("dcieslak19973.slackr", "w1:p1", None),
            [
                "plugin",
                "pane",
                "open",
                "--plugin",
                "dcieslak19973.slackr",
                "--entrypoint",
                super::entrypoint(),
                "--placement",
                "split",
                "--target-pane",
                "w1:p1",
                "--direction",
                "right",
                "--focus",
            ]
        );
    }

    #[test]
    fn open_argv_appends_cwd_only_when_present() {
        let argv = open_argv("id", "p", Some("/repo"));
        assert_eq!(argv[argv.len() - 2..], ["--cwd".to_string(), "/repo".to_string()]);
        assert!(!open_argv("id", "p", None).contains(&"--cwd".to_string()));
    }

    #[test]
    fn entrypoint_matches_the_platform_pane_id() {
        let expected = if cfg!(windows) { "feed-windows" } else { "feed" };
        assert_eq!(super::entrypoint(), expected);
    }

    #[test]
    fn opened_pane_id_reads_the_plugin_pane_open_response() {
        let json = r#"{"result":{"plugin_pane":{"pane":{"pane_id":"w1:p7"}}}}"#;
        assert_eq!(opened_pane_id(json).as_deref(), Some("w1:p7"));
        assert_eq!(opened_pane_id(r#"{"result":{}}"#), None);
        assert_eq!(opened_pane_id("not json"), None);
    }
}
