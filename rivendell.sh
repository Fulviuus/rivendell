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

# The watcher is a separate program that holds the long poll for an awake
# agent. awake.rs looks for it beside the app's own binary, so it is copied
# there rather than declared in externalBin — that would make it a *build-time*
# requirement, and `cargo test` would fail on a checkout that has not built it.
build_watcher() {
  bold "Building the watcher"
  cargo build --release --manifest-path runner/Cargo.toml
  # Installed in the same breath, always. Building without installing leaves a
  # stale copy beside the app, and the app hands that copy's path to agents —
  # so the drift shows up as an agent being told to run a flag its binary
  # rejects. Three separate bugs in this project have been a stale artifact.
  install_watcher
}

# The stdio bridge, which is also the channel that pushes into a session.
# Built for the same reason as the watcher: a test drives the real binary, and
# a stale one would quietly decide whether that test passes.
build_bridge() {
  bold "Building the bridge"
  cargo build --release --manifest-path mcp-shim/Cargo.toml
}

# Both places the app can be run from: the dev binary and the bundle.
install_watcher() {
  local src="runner/target/release/rivendell-run" n=0
  [ -x "$src" ] || die "the watcher was not built — run './rivendell.sh' rather than tauri directly"
  for dir in src-tauri/target/debug src-tauri/target/release \
             "src-tauri/target/debug/${BUNDLE_REL}/Contents/MacOS" \
             "src-tauri/target/release/${BUNDLE_REL}/Contents/MacOS"; do
    if [ -d "$dir" ]; then
      # Removed first, deliberately. Overwriting a Mach-O in place on Apple
      # silicon invalidates its signature and the kernel then SIGKILLs it —
      # which presents as a binary that exits 137 with no output and no
      # explanation. A fresh inode keeps the signature cargo gave it.
      rm -f "$dir/rivendell-run"
      cp "$src" "$dir/rivendell-run"
      n=$((n + 1))
    fi
  done
  info "watcher installed in ${n} place(s)"
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

APP_DATA="$HOME/Library/Application Support/dev.fulvio.rivendell"

# Agents the app started run in their own process groups so one signal reaches
# the whole tree. That teardown happens on an orderly quit — but this script
# kills the app outright, which skips it, and macOS then reparents the children
# and lets them keep running. And keep billing. The app writes down what it
# started; this reads that back.
stop_awake_agents() {
  local ledger="${APP_DATA}/running.json"
  [ -f "$ledger" ] || return 0
  local pgids
  pgids="$(sed -n 's/.*"pgid":[[:space:]]*\([0-9]*\).*/\1/p' "$ledger" | sort -u)"
  for pgid in $pgids; do
    kill -0 "-${pgid}" 2>/dev/null || continue
    info "stopping an agent the app started (process group ${pgid})"
    kill -TERM "-${pgid}" 2>/dev/null || true
  done
  rm -f "$ledger"
}

# `open` on an already-running app just brings it to the front — it will NOT
# pick up a new build. Quitting first is the whole reason this exists.
#
# Both directions are cleared every time: a bundled app and a dev run collide
# on the same MCP port, so leaving either behind breaks the other.
stop_running() {
  stop_awake_agents
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
    build_watcher
    stop_running
    # Before tauri starts it: the dev binary looks beside itself.
    mkdir -p src-tauri/target/debug && install_watcher
    bold "Starting with hot reload — ctrl-c to stop"
    info "the Rust side rebuilds on change; the UI reloads instantly"
    exec npm run tauri dev
    ;;

  run)
    preflight
    build_watcher
    bold "Building ${APP_NAME} (debug)"
    npm run tauri build -- --debug --bundles app
    install_watcher
    stop_running
    launch "src-tauri/target/debug/${BUNDLE_REL}"
    ;;

  release)
    preflight
    build_watcher
    bold "Building ${APP_NAME} (release — slower, optimised)"
    npm run tauri build -- --bundles app
    install_watcher
    stop_running
    launch "src-tauri/target/release/${BUNDLE_REL}"
    ;;

  dmg)
    preflight
    build_watcher
    bold "Building ${APP_NAME} and packaging a .dmg"
    npm run tauri build -- --bundles app,dmg
    install_watcher
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
    # Two end-to-end tests drive real binaries, so build them first —
    # otherwise a stale one silently decides whether the suite passes.
    build_watcher
    build_bridge
    bold "Rust"
    cargo test --manifest-path src-tauri/Cargo.toml
    bold "Runner"
    cargo test --manifest-path runner/Cargo.toml
    ;;

  -h|--help|help)
    sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    ;;

  *)
    die "unknown command '${1}'. Try: ./rivendell.sh --help"
    ;;
esac
