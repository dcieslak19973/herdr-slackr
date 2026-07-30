# Windows support — implementation plan

Design: `../specs/2026-07-30-windows-support-design.md`. One PR, one commit per step where
sensible; full gate (`just ci`) green before the PR.

- [ ] **Step 1 — sidebar subcommand:** new `src/sidebar.rs` (pure decision/parse functions +
      unit tests; thin runner shelling out to `$HERDR_BIN_PATH`), dispatch from `src/cli.rs`
      (`owns`, `run`, USAGE line), platform-selected pane entrypoint (`feed` /
      `feed-windows`). Integration tests: usage exit 2; no-workspace refusal exit 1.
- [ ] **Step 2 — Windows runtime fixes:** `src/proc.rs::on_path` PATHEXT probe;
      `src/browser.rs` rundll32 opener.
- [ ] **Step 3 — installer + manifest:** `herdr/install.ps1`; delete `herdr/sidebar.sh`;
      rewrite `herdr-plugin.toml` (platforms, min_herdr_version 0.7.5, per-platform build
      twins, `-windows` pane/action twins, unix actions → `sidebar <mode>` one-liners).
      Validate with `herdr plugin link … --disabled` on this machine.
- [ ] **Step 4 — CI/release:** `release.yml` + x86_64-pc-windows-msvc (zip); `ci.yml` +
      windows-latest `cargo test` job.
- [ ] **Step 5 — docs:** README (Windows section + troubleshooting), CHANGELOG (Unreleased),
      spec cross-references.
- [ ] **Step 6 — verify:** `just ci` on Windows; manifest link check; sidebar refusal smoke;
      PR with live-smoke checklist (pane open via linked plugin; post-release install test).
