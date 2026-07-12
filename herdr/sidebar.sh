#!/usr/bin/env bash
# The slackr pane actions (specs/slack-host.md#pane-actions), adapted from herdr-reviewr's
# sidebar.sh. Two differences from reviewr's script: there is no `--resolve-plugin-config` step
# (slackr's config.toml has no placement/theme keys sidebar.sh needs — see specs/config.md — it
# is read only by the pane binary itself), and placement is fixed (`split`, `right`) rather than
# user-configurable, since one feed pane has no reason to open as a tab or overlay.
#
#   sidebar.sh toggle   open the feed pane, or close it if open
#   sidebar.sh open     open the feed pane, no-op if one is open
#   sidebar.sh close    close every slackr pane, no-op if none
#
# The workspace's feed pane is any pane labeled "slack" in the live pane list (the manifest's
# `[[panes]] title`). There is no state file. Actions refuse loudly (exit 1, one stderr line) and
# report successes on stdout.
set -uo pipefail

# herdr runs plugin commands with a minimal PATH; ensure jq resolves on common installs.
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:${PATH:-}"

mode="${1:-toggle}"
H="${HERDR_BIN_PATH:-herdr}"

refuse() {
  printf 'slackr: %s\n' "$1" >&2
  exit 1
}

case "$mode" in
toggle | open | close) ;;
*) refuse "unknown mode '$mode' (toggle | open | close)" ;;
esac

ws="${HERDR_WORKSPACE_ID:-}"
pane="${HERDR_PANE_ID:-}"
cwd=""
[ -n "${HERDR_PLUGIN_CONTEXT_JSON:-}" ] &&
  cwd=$(printf '%s' "$HERDR_PLUGIN_CONTEXT_JSON" | jq -r '.focused_pane_cwd // .workspace_cwd // empty' 2>/dev/null)

[ -n "$ws" ] || refuse "no workspace context (invoke from inside herdr)"

# One pane-list snapshot serves the whole run. A failed listing must not read as
# "no feed pane" — that would stack a duplicate on toggle and false-succeed a close.
panes_json=$("$H" pane list --workspace "$ws" 2>/dev/null) && [ -n "$panes_json" ] ||
  refuse "herdr pane list failed for $ws"

# The workspace's feed pane: every "slack"-labeled pane, any tab (spec: one feed per workspace).
existing=$(printf '%s' "$panes_json" | jq -r '.result.panes[] | select(.label == "slack") | .pane_id' 2>/dev/null)

# Plain `pane close`, not `plugin pane close`: the plugin-pane registry does not
# survive a herdr restart and would strand the pane.
close_all() {
  closed="" failed=""
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if "$H" pane close "$p" >/dev/null 2>&1; then closed="$closed $p"; else failed="$failed $p"; fi
  done <<EOF
$existing
EOF
  [ -z "$failed" ] || refuse "failed to close$failed in $ws"
  printf 'closed%s in %s\n' "$closed" "$ws"
}

case "$mode" in
close)
  [ -n "$existing" ] || { printf 'close: nothing open in %s\n' "$ws"; exit 0; }
  close_all
  exit 0
  ;;
toggle)
  if [ -n "$existing" ]; then
    close_all
    exit 0
  fi
  ;;
open)
  if [ -n "$existing" ]; then
    printf 'open: already open (%s) in %s\n' "$(printf '%s' "$existing" | tr '\n' ' ' | sed 's/ $//')" "$ws"
    exit 0
  fi
  ;;
esac

# Opening from here on. Focus follows a split open (the feed is a companion pane, not a
# full-attention view — unlike reviewr's configurable placement, this is fixed).
if [ -z "$pane" ]; then
  pane=$(printf '%s' "$panes_json" | jq -r '.result.panes[0].pane_id // empty' 2>/dev/null)
fi
[ -n "$pane" ] || refuse "no pane to attach to in $ws"

set -- --plugin "${HERDR_PLUGIN_ID:-dcieslak19973.slackr}" --entrypoint feed \
  --placement split --target-pane "$pane" --direction right --focus
[ -n "$cwd" ] && set -- "$@" --cwd "$cwd"

new=$("$H" plugin pane open "$@" 2>/dev/null | jq -r '.result.plugin_pane.pane.pane_id // empty' 2>/dev/null)
[ -n "$new" ] || refuse "herdr plugin pane open failed"
printf 'opened %s (split) in %s\n' "$new" "$ws"
