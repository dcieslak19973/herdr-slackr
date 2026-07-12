//! Smoke tests for the `herdr-slackr` binary: `--version` and the
//! "must run inside herdr" guard when launched outside a herdr pane.

use std::process::Command;

#[test]
fn version_flag_prints_name_and_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-slackr"))
        .arg("--version")
        .output()
        .expect("failed to spawn herdr-slackr");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout is not valid UTF-8");
    assert!(stdout.starts_with("herdr-slackr 0.1"), "unexpected stdout: {stdout:?}");
}

#[test]
fn bare_invocation_outside_herdr_exits_with_guard_message() {
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-slackr"))
        .env_remove("HERDR_PLUGIN_CONFIG_DIR")
        .output()
        .expect("failed to spawn herdr-slackr");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("stderr is not valid UTF-8");
    assert!(
        stderr
            .contains("herdr-slackr: the pane must run inside herdr (set HERDR_PLUGIN_CONFIG_DIR)"),
        "unexpected stderr: {stderr:?}"
    );
}
