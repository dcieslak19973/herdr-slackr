# Windows support for herdr-slackr

Date: 2026-07-30
Status: approved

Sibling of herdr-reviewr's `docs/superpowers/specs/2026-07-30-windows-support-design.md` (same
diagnosis, same shape). Slackr-specific deltas: no `worktree.created` event, no placement
config (fixed right split), no clipboard export — the Windows-runtime gaps here are the URL
opener and the `PATH` probe. Facts verified after that spec was approved are marked **(new)**.

## Problem

`herdr plugin install dcieslak19973/herdr-slackr` fails or produces a dead plugin on native
Windows. Two distinct failures:

1. **The reported error** — `Error { kind: NotFound, message: "program not found" }` — is
   herdr failing to spawn `git` for the clone when git is not on `PATH`. Reproduced exactly on
   this machine by stripping git from `PATH`. Upstream herdr issue; the plugin can only
   document it (README troubleshooting).
2. **The plugin's own gap** — when git is present the install "succeeds", but herdr skips the
   `[[build]]` step on Windows (`build (skipped on windows)`), so no binary is downloaded, and
   every pane/action command routes through `bash`/`sh`, which native Windows lacks. The
   manifest declares `platforms = ["macos", "linux"]` and no Windows release asset exists.

## Verified facts this design rests on

- herdr ships and runs on Windows; `plugin install` works end-to-end there (0.7.1-preview and
  0.7.5-preview, verified locally on 2026-07-30).
- Item-level `platforms` on `[[build]]`/`[[panes]]`/`[[actions]]` entries is honored by
  0.7.5-preview (probe plugin linked and listed, entries carry their filters). Commands are
  argv arrays, never shell-expanded.
- **(new)** herdr rejects duplicate action/pane ids even across disjoint platform filters
  (`duplicate action id 'toggle'`, probed on 0.7.5-preview; community plugins herdr-lazy and
  herdr-file-viewer document the same) — Windows variants need distinct ids.
- **(new)** On Windows herdr reports/sets the plugin root as a `\\?\` verbatim path and hands
  relative commands straight to `CreateProcessW` (no `.exe` appending, no cwd-relative
  resolution — herdr-file-viewer GH #58). Windows commands therefore route through
  `powershell -Command`, expand `$env:HERDR_PLUGIN_ROOT` themselves, and strip the `\\?\`
  prefix before use.
- The crate compiles and tests cleanly on Windows (this machine). `cfg(unix)` sites
  (`src/tokens.rs`, `src/users_cache.rs`, `src/cli.rs` symlink fallback) already degrade
  correctly. The REST layer shells out to `curl`, which Windows 10 1803+ ships in System32.

## Design

### 1. Release pipeline

Add `x86_64-pc-windows-msvc` (runs-on `windows-latest`, `.zip` + `.sha256` sidecar — taiki-e's
Windows defaults) to `.github/workflows/release.yml`. No Windows-ARM target until requested
(x64 emulation covers it). CI gains a `windows-latest` job running `cargo test`.

### 2. Windows installer: `herdr/install.ps1`

PowerShell port of `herdr/install.sh`, run by a Windows-only `[[build]]` variant:

- Resolve the plugin root from `$PSScriptRoot`; read `version` from `herdr-plugin.toml`.
- Download `herdr-slackr-x86_64-pc-windows-msvc.zip` + `.sha256` sidecar from the matching
  release, with retries (release assets are eventually-consistent, incl. 404s).
- Verify via `Get-FileHash -Algorithm SHA256`; extract `bin\herdr-slackr.exe`.
- No PATH mutation (no `~/.local/bin` convention on Windows): print the absolute binary path
  and the same next-steps epilogue as install.sh.
- `$ErrorActionPreference = 'Stop'` for `set -euo pipefail` parity.

### 3. Pane actions move into the binary

New subcommand `herdr-slackr sidebar <toggle|open|close>`, a 1:1 port of `sidebar.sh`
semantics (one feed pane per workspace, label `slack`, fixed `split`/`right`, focus follows a
manual open; refusals are one `slackr: …` stderr line + exit 1, successes one stdout line):

- Context from `HERDR_WORKSPACE_ID`/`HERDR_PANE_ID`/`HERDR_PLUGIN_ID`;
  `HERDR_PLUGIN_CONTEXT_JSON` parsed with `serde_json` (`.focused_pane_cwd` //
  `.workspace_cwd`). No `jq` anywhere.
- Pane orchestration shells out to `$HERDR_BIN_PATH` (fallback `herdr`) for `pane list`,
  plain `pane close` (the plugin-pane registry does not survive a herdr restart), and
  `plugin pane open`.
- **(new)** The open passes the platform's own pane entrypoint: `feed` on unix,
  `feed-windows` on Windows (`cfg!(windows)`), since duplicate pane ids are rejected.
- Decision logic (mode × existing panes → planned herdr invocations) is pure functions with
  unit tests, matching the repo's test style (`herdr_meta.rs` precedent).
- `herdr/sidebar.sh` is **deleted**; the `jq` runtime dependency disappears on all platforms.

### 4. Manifest: per-platform command variants

- Top-level `platforms = ["macos", "linux", "windows"]`; `min_herdr_version = "0.7.5"` — the
  earliest version verified to honor item-level `platforms` (0.7.1–0.7.4 behavior with a
  two-`[[build]]` manifest is unverifiable here; a wrong guess breaks unix installs, and
  0.7.1-era herdrs refuse cleanly on `min_herdr_version` and say why).
- `[[build]]`, the `feed` pane, and the four actions each keep their unix entry
  (`platforms = ["macos", "linux"]`, actions becoming `bash -c` one-liners invoking
  `…/bin/herdr-slackr sidebar <mode>`) and gain a Windows twin (`platforms = ["windows"]`,
  ids suffixed `-windows`) routing through
  `powershell -NoProfile -ExecutionPolicy Bypass -Command` with the `\\?\` strip.
- PowerShell over `cmd /c` because it quotes paths containing spaces correctly
  (`C:\Users\Dan Cieslak` is the local proof case).

### 5. Windows URL opener

- `src/browser.rs`: on Windows open URLs via `rundll32 url.dll,FileProtocolHandler <url>` —
  argv-only (no `cmd` metacharacter mangling of `&` in permalinks), exits 0 (unlike
  `explorer.exe`, which exits 1 unconditionally).
- `src/proc.rs::on_path`: probe `PATHEXT` suffixes on Windows (`rundll32` is `rundll32.exe`
  on disk, so the unix-style literal probe never matches).

### 6. Documentation

- README: Windows install prerequisites (git on `PATH`; herdr ≥ 0.7.5), a troubleshooting
  entry mapping `Error { kind: NotFound, message: "program not found" }` → git missing from
  `PATH`, and a note that a pre-Windows-support herdr skips the build step (binary-less
  install) and how to tell.
- CHANGELOG under Unreleased; `specs/pane.md`/`specs/agent-cli.md` gain the `sidebar`
  subcommand where the actions are described.
- Upstream (outside this repo): file herdr issues for the raw git-spawn error and for
  `plugin unlink` failing with a raw `NotFound` os error (observed 0.7.5-preview).

### 7. Testing

- Unit tests for the sidebar decision functions and context JSON parsing; integration tests
  for `sidebar` arg/refusal behavior (no herdr server needed: the workspace-context refusal
  fires first).
- CI runs `cargo test` on `windows-latest`.
- End-to-end (post-merge, pre-release): `herdr plugin link` on this Windows machine — pane
  opens, toggle/open/close actions work. The download path is only fully testable after the
  first release that ships a Windows asset; the colleague's scenario is the acceptance test.

## Risks

- **Older herdr**: 0.7.1–0.7.4 behavior with per-platform `[[build]]` twins is unknown;
  mitigated by the `min_herdr_version` bump (herdr refuses with a version message rather than
  half-installing).
- **TUI on Windows terminals**: ratatui/crossterm is well-supported in Windows Terminal; the
  pane gets a live smoke test before release.
- **PowerShell startup latency** on actions (~100–300 ms) is accepted; actions are
  user-initiated and infrequent.

## Out of scope

- Windows-ARM release target; OSC 52 clipboard; any change to herdr itself.
- The `[[actions]]`/`[[panes]]` unix ids and behavior — existing keybindings keep working.
