#!/usr/bin/env bash
#
# Build and launch Rivendell.
#
#   ./rivendell.sh            build a debug bundle and launch it
#   ./rivendell.sh dev        run with hot reload (stays in the foreground)
#   ./rivendell.sh release    optimised build, then launch
#   ./rivendell.sh dmg        optimised build plus a .dmg, no launch
#   ./rivendell.sh stop       quit anything this project has running
#   ./rivendell.sh test       the whole test suite, Rust and TypeScript
#
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

APP_NAME="Rivendell"
BIN_NAME="rivendell"          # the unbundled binary cargo produces
BUNDLE_REL="bundle/macos/${APP_NAME}.app"
# Any process whose path contains this is a bundled instance we started.
RUNNING_PATTERN="${APP_NAME}.app/Contents/MacOS/"

PROJECT_DIR="$(pwd -P)"
# Read the port from the Vite config so the two cannot drift apart. It is
# strictPort, so a leftover dev server is a hard failure rather than a shrug.
VITE_PORT="$(sed -n 's/.*port: *\([0-9][0-9]*\).*/\1/p' vite.config.ts | head -1)"
VITE_PORT="${VITE_PORT:-1420}"

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

wait_gone() {
  local pattern="$1"
  for _ in $(seq 1 40); do
    pgrep -f "$pattern" >/dev/null 2>&1 || return 0
    sleep 0.25
  done
  pkill -9 -f "$pattern" 2>/dev/null || true
}

pids_on_vite_port() { lsof -tiTCP:"$VITE_PORT" -sTCP:LISTEN 2>/dev/null || true; }
cwd_of()            { lsof -a -p "$1" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -1; }

# A dev run leaves two things behind: the unbundled binary and Vite. Vite
# outlives `tauri dev` if that is killed rather than ctrl-c'd, and because the
# port is strict the next dev run then dies on "port already in use".
stop_dev() {
  if pgrep -f "${PROJECT_DIR}/src-tauri/target/.*/${BIN_NAME}$" >/dev/null 2>&1; then
    info "stopping the dev binary"
    pkill -f "${PROJECT_DIR}/src-tauri/target/.*/${BIN_NAME}$" || true
    wait_gone "${PROJECT_DIR}/src-tauri/target/.*/${BIN_NAME}$"
  fi

  local pids
  pids="$(pids_on_vite_port)"
  [ -z "$pids" ] && return 0

  for pid in $pids; do
    # Only ever kill a dev server belonging to this checkout. Someone else's
    # process on the same port is their business, and worth saying so.
    if [ "$(cwd_of "$pid")" = "$PROJECT_DIR" ]; then
      info "freeing port ${VITE_PORT} (stale dev server, pid ${pid})"
      kill "$pid" 2>/dev/null || true
    else
      die "port ${VITE_PORT} is held by pid ${pid}, which is not this project.
  Stop it yourself, or change the port in vite.config.ts."
    fi
  done

  for _ in $(seq 1 40); do
    [ -z "$(pids_on_vite_port)" ] && return 0
    sleep 0.25
  done
  die "port ${VITE_PORT} did not free up"
}

# `open` on an already-running app just brings it to the front — it will NOT
# pick up a new build. Quitting first is the whole reason this exists.
#
# Both directions are cleared every time: a bundled app and a dev run collide
# on the same MCP port, so leaving either behind breaks the other.
stop_running() {
  if pgrep -f "$RUNNING_PATTERN" >/dev/null 2>&1; then
    info "stopping the running instance"
    pkill -f "$RUNNING_PATTERN" || true
    wait_gone "$RUNNING_PATTERN"
  fi
  stop_dev
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

  stop)
    stop_running
    bold "Stopped"
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
    sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    ;;

  *)
    die "unknown command '${1}'. Try: ./rivendell.sh --help"
    ;;
esac
