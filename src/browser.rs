//! Open a URL in the user's browser — the `o` key's only outward action. Copied from
//! `herdr-reviewr`'s `src/browser.rs` (same platform-opener probe: the first of `open`/
//! `xdg-open` found on `PATH` wins; a clear error when neither is present).

use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// Platform openers, tried in order: macOS `open`, then the Linux `xdg-open`.
const OPENERS: &[&str] = &["open", "xdg-open"];

/// Open `url` in the default browser via the first available opener. Errors when none is on
/// `PATH` (the caller surfaces it to the status line). The opener hands the URL to the browser
/// and exits at once, so this waits for it — reaping the child rather than leaving a zombie,
/// and returning fast enough for a key handler.
pub fn open(url: &str) -> Result<()> {
    let tool = OPENERS
        .iter()
        .copied()
        .find(|t| crate::proc::on_path(t))
        .context("no URL opener found (need `open` or `xdg-open`)")?;
    let status = Command::new(tool)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("spawning {tool}"))?;
    if !status.success() {
        anyhow::bail!("{tool} failed to open the URL");
    }
    Ok(())
}
