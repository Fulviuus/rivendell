#!/usr/bin/env bash
#
# Build and launch Rivendell.
#
#   ./rivendell.sh            build a debug bundle and launch it
#   ./rivendell.sh dev        run with hot reload (stays in the foreground)
#   ./rivendell.sh release    optimised build, then launch
#   ./rivendell.sh dmg        optimised build plus a .dmg, no launch
#   ./rivendell.sh test       the whole test suite, Rust and TypeScript
#
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

APP_NAME="Rivendell"
BUNDLE_REL="bundle/macos/${APP_NAME}.app"
# Any process whose path contains this is an instance we started.
RUNNING_PATTERN="${APP_NAME}.app/Contents/MacOS/"

bold()  { printf '\033[1m%s\033[0m\n' "$*"; }
info()  { printf '\033[2m  %s\033[0m\n' "$*"; }
die()   { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

require() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is not installed. $2"
}

preflight() {
  require node  "Install Node 20+ from https://nodejs.org"
  require cargo "Install Rust from https://rustup.rs"
  [ "$(uname -s)" = "Darwin" ] || die "this script targets macOS; on other platforms use 'npm run tauri build' directly"

  if [ ! -d node_modules ]; then
    bold "Installing npm dependencies"
    npm install
  fi
}

# `open` on an already-running app just brings it to the front — it will NOT
# pick up a new build. Quitting first is the whole reason this exists.
stop_running() {
  if pgrep -f "$RUNNING_PATTERN" >/dev/null 2>&1; then
    info "stopping the running instance"
    pkill -f "$RUNNING_PATTERN" || true
    for _ in $(seq 1 40); do
      pgrep -f "$RUNNING_PATTERN" >/dev/null 2>&1 || break
      sleep 0.25
    done
    pgrep -f "$RUNNING_PATTERN" >/dev/null 2>&1 && pkill -9 -f "$RUNNING_PATTERN" || true
  fi
}

# The MCP port is chosen at startup from a small range, so probe rather than
# assume. Also confirms the backend actually came up, not just the window.
report_server() {
  for _ in $(seq 1 40); do
    for port in 8787 8788 8789 8790 8791; do
      if curl -fsS --max-time 1 "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then
        bold "Running"
        info "MCP server: http://127.0.0.1:${port}/mcp"
        info "point an agent at it with the key from Agents & keys"
        return 0
      fi
    done
    sleep 0.5
  done
  printf '\033[33mwarning:\033[0m the app launched but its MCP server did not answer.\n' >&2
  info "check Console.app, or run './rivendell.sh dev' to see the logs"
}

launch() {
  local app="$1"
  [ -d "$app" ] || die "no bundle at $app — the build did not produce one"
  bold "Launching ${APP_NAME}"
  open "$app"
  report_server
}

case "${1:-run}" in
  dev)
    preflight
    stop_running
    bold "Starting with hot reload — ctrl-c to stop"
    info "the Rust side rebuilds on change; the UI reloads instantly"
    exec npm run tauri dev
    ;;

  run)
    preflight
    bold "Building ${APP_NAME} (debug)"
    npm run tauri build -- --debug --bundles app
    stop_running
    launch "src-tauri/target/debug/${BUNDLE_REL}"
    ;;

  release)
    preflight
    bold "Building ${APP_NAME} (release — slower, optimised)"
    npm run tauri build -- --bundles app
    stop_running
    launch "src-tauri/target/release/${BUNDLE_REL}"
    ;;

  dmg)
    preflight
    bold "Building ${APP_NAME} and packaging a .dmg"
    npm run tauri build -- --bundles app,dmg
    dmg=$(find src-tauri/target/release/bundle/dmg -name '*.dmg' -maxdepth 1 2>/dev/null | head -1)
    [ -n "$dmg" ] || die "no .dmg was produced"
    bold "Packaged"
    info "$dmg"
    # Unsigned builds are quarantined on any machine that did not build them.
    info "not notarised — recipients need: xattr -dr com.apple.quarantine /Applications/${APP_NAME}.app"
    ;;

  test)
    preflight
    bold "TypeScript"
    npx tsc --noEmit
    info "clean"
    bold "Rust"
    cargo test --manifest-path src-tauri/Cargo.toml
    ;;

  -h|--help|help)
    sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    ;;

  *)
    die "unknown command '${1}'. Try: ./rivendell.sh --help"
    ;;
esac
