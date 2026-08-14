#!/usr/bin/env bash
set -euo pipefail

MODE="${1:-run}"
APP_NAME="SayTrace"
BUNDLE_ID="com.localtranscript.desktop"
DEFAULT_EXECUTABLE="local-transcript"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_KIND="release"
if [[ "$MODE" == "--debug" || "$MODE" == "debug" || "$MODE" == "--dev" || "$MODE" == "dev" ]]; then
  BUILD_KIND="debug"
fi
TARGET_DIR="$ROOT_DIR/src-tauri/target/$BUILD_KIND"
APP_BUNDLE="$TARGET_DIR/bundle/macos/$APP_NAME.app"

# Codex and other GUI launchers do not always inherit the interactive shell PATH.
export PATH="/opt/homebrew/opt/node@22/bin:/opt/homebrew/opt/rustup/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
export CARGO_TARGET_DIR="$ROOT_DIR/src-tauri/target"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-15.0}"

usage() {
  echo "usage: $0 [run|--performance|--dev|--debug|--logs|--telemetry|--verify]" >&2
}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

stop_running_app() {
  /usr/bin/pkill -x "$APP_NAME" >/dev/null 2>&1 || true
  /usr/bin/pkill -x "$DEFAULT_EXECUTABLE" >/dev/null 2>&1 || true
}

bundle_executable() {
  local executable
  executable="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$APP_BUNDLE/Contents/Info.plist")"
  printf '%s' "$APP_BUNDLE/Contents/MacOS/$executable"
}

open_app() {
  /usr/bin/open -n "$APP_BUNDLE"
}

resolve_codesign_identity() {
  if [[ -n "${SAYTRACE_CODESIGN_IDENTITY:-}" ]]; then
    printf '%s' "$SAYTRACE_CODESIGN_IDENTITY"
    return
  fi

  local identity
  identity="$(
    /usr/bin/security find-identity -v -p codesigning 2>/dev/null |
      /usr/bin/awk '/"Apple Development:/ { print $2; exit }'
  )"
  if [[ -n "$identity" ]]; then
    printf '%s' "$identity"
  else
    printf '%s' '-'
  fi
}

case "$MODE" in
  run|--performance|performance|--dev|dev|--debug|debug|--logs|logs|--telemetry|telemetry|--verify|verify)
    ;;
  *)
    usage
    exit 2
    ;;
esac

if [[ $# -gt 1 ]]; then
  usage
  exit 2
fi

require_command node
require_command npm
require_command cargo

if [[ "$(/usr/bin/uname -s)" != "Darwin" || "$(/usr/bin/uname -m)" != "arm64" ]]; then
  echo "SayTrace's MLX development build requires Apple Silicon macOS." >&2
  exit 1
fi

if [[ ! -d "$ROOT_DIR/node_modules" ]]; then
  echo "JavaScript dependencies are missing; run 'npm ci' first." >&2
  exit 1
fi

if [[ ! -x "$ROOT_DIR/worker/.venv/bin/python" ]]; then
  echo "warning: worker/.venv is missing; the app can launch, but the local worker will remain offline." >&2
  echo "run 'uv sync --project worker --extra ml --group dev' for MLX inference." >&2
fi

if ! command -v ffmpeg >/dev/null 2>&1 || ! command -v ffprobe >/dev/null 2>&1; then
  echo "warning: FFmpeg/FFprobe are missing; imports and transcription will not work." >&2
fi

stop_running_app

cd "$ROOT_DIR"
TAURI_BUILD_ARGS=(--bundles app --features development-runtime)
if [[ "$BUILD_KIND" == "debug" ]]; then
  TAURI_BUILD_ARGS=(--debug "${TAURI_BUILD_ARGS[@]}")
fi
echo "Building the $BUILD_KIND macOS app bundle"
npm run tauri:build -- "${TAURI_BUILD_ARGS[@]}"

if [[ ! -d "$APP_BUNDLE" ]]; then
  echo "Tauri completed without producing $APP_BUNDLE" >&2
  exit 1
fi

APP_BINARY="$(bundle_executable)"
EXECUTABLE_NAME="${APP_BINARY##*/}"

if [[ ! -x "$APP_BINARY" ]]; then
  echo "app executable is missing or not executable: $APP_BINARY" >&2
  exit 1
fi

# Prefer an installed Apple Development identity so the designated requirement
# remains stable across rebuilds. An ad-hoc signature is tied to the binary's
# changing CDHash, which leaves apparently enabled TCC entries stale.
CODESIGN_IDENTITY="$(resolve_codesign_identity)"
if [[ "$CODESIGN_IDENTITY" == "-" ]]; then
  echo "warning: no Apple Development identity found; using ad-hoc signing." >&2
  echo "warning: macOS capture permissions may need to be reset after each rebuild." >&2
else
  echo "Signing SayTrace with stable Apple Development identity $CODESIGN_IDENTITY"
fi

/usr/bin/codesign \
  --force \
  --deep \
  --sign "$CODESIGN_IDENTITY" \
  --timestamp=none \
  --options runtime \
  --entitlements "$ROOT_DIR/src-tauri/Entitlements.plist" \
  "$APP_BUNDLE"
/usr/bin/codesign --verify --deep --strict "$APP_BUNDLE"

case "$MODE" in
  run|--performance|performance|--dev|dev)
    open_app
    ;;
  --debug|debug)
    exec /usr/bin/lldb -- "$APP_BINARY"
    ;;
  --logs|logs)
    open_app
    exec /usr/bin/log stream --info --style compact \
      --predicate "process == \"$EXECUTABLE_NAME\""
    ;;
  --telemetry|telemetry)
    open_app
    exec /usr/bin/log stream --info --style compact \
      --predicate "subsystem == \"$BUNDLE_ID\""
    ;;
  --verify|verify)
    open_app
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      if /usr/bin/pgrep -f "$APP_BINARY" >/dev/null 2>&1; then
        echo "$APP_NAME launched successfully."
        exit 0
      fi
      sleep 1
    done
    echo "$APP_NAME did not remain running after launch." >&2
    exit 1
    ;;
esac
