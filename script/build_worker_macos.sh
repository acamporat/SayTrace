#!/bin/bash
set -euo pipefail

usage() {
  cat <<'EOF'
Build and stage the Apple Silicon SayTrace worker runtime.

Usage: script/build_worker_macos.sh [--stage-only] [--skip-sync]

Options:
  --stage-only  Reuse build/pyinstaller-macos/dist/local-transcript-worker.
  --skip-sync   Do not run uv sync before invoking PyInstaller.
  -h, --help    Show this help.

Environment:
  LOCAL_TRANSCRIPT_FFMPEG       self-contained arm64 ffmpeg (defaults to PATH lookup)
  LOCAL_TRANSCRIPT_FFPROBE      self-contained arm64 ffprobe (defaults to PATH lookup)
  LOCAL_TRANSCRIPT_UV           uv executable (defaults to PATH lookup)

Output:
  build/macos-runtime/runtime/
EOF
}

STAGE_ONLY=0
SKIP_SYNC=0
while (($#)); do
  case "$1" in
    --stage-only)
      STAGE_ONLY=1
      ;;
    --skip-sync)
      SKIP_SYNC=1
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "The macOS runtime must be built natively on Apple Silicon." >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
WORKER_ROOT="$REPOSITORY_ROOT/worker"
PYINSTALLER_ROOT="$REPOSITORY_ROOT/build/pyinstaller-macos"
WORKER_BUNDLE="$PYINSTALLER_ROOT/dist/local-transcript-worker"
RUNTIME_PARENT="$REPOSITORY_ROOT/build/macos-runtime"
RUNTIME_OUTPUT="$RUNTIME_PARENT/runtime"
MODEL_MANIFEST="$WORKER_ROOT/model-manifest.macos.json"
MANIFEST_HELPER="$SCRIPT_DIR/generate_macos_runtime_manifest.py"

find_executable() {
  local configured="$1"
  local fallback_name="$2"
  if [[ -n "$configured" ]]; then
    if [[ ! -x "$configured" ]]; then
      echo "Configured $fallback_name is not executable: $configured" >&2
      return 1
    fi
    printf '%s\n' "$configured"
    return
  fi
  command -v "$fallback_name" || {
    echo "$fallback_name was not found on PATH." >&2
    return 1
  }
}

FFMPEG="$(find_executable "${LOCAL_TRANSCRIPT_FFMPEG:-}" ffmpeg)"
FFPROBE="$(find_executable "${LOCAL_TRANSCRIPT_FFPROBE:-}" ffprobe)"

for executable in "$FFMPEG" "$FFPROBE"; do
  if ! file -L "$executable" | grep -q 'arm64'; then
    echo "Release media tool is not an arm64 Mach-O executable: $executable" >&2
    exit 1
  fi
done

FFMPEG_VERSION_OUTPUT="$("$FFMPEG" -version 2>&1)"
FFMPEG_VERSION_LINE="${FFMPEG_VERSION_OUTPUT%%$'\n'*}"
if [[ "$FFMPEG_VERSION_OUTPUT" == *"--enable-nonfree"* ]]; then
  echo "Refusing to package an FFmpeg build configured with --enable-nonfree." >&2
  exit 1
fi
if [[ "$FFMPEG_VERSION_OUTPUT" == *"--enable-gpl"* ]]; then
  echo "Refusing to package a GPL-enabled FFmpeg build; provide a redistributable LGPL-compatible build." >&2
  exit 1
fi
EXTERNAL_FFMPEG_LIBRARIES="$({ otool -L "$FFMPEG"; otool -L "$FFPROBE"; } \
  | awk '$1 ~ /^\// && $1 !~ /:$/ { print $1 }' \
  | grep '^/' \
  | grep -Ev '^(/usr/lib/|/System/Library/)' \
  | sort -u || true)"
if [[ -n "$EXTERNAL_FFMPEG_LIBRARIES" ]]; then
  echo "Refusing media tools linked to libraries outside macOS system paths:" >&2
  echo "$EXTERNAL_FFMPEG_LIBRARIES" >&2
  echo "Provide self-contained redistributable FFmpeg/FFprobe binaries." >&2
  exit 1
fi

if ((STAGE_ONLY == 0)); then
  UV="$(find_executable "${LOCAL_TRANSCRIPT_UV:-}" uv)"
  if ((SKIP_SYNC == 0)); then
    "$UV" sync \
      --project "$WORKER_ROOT" \
      --frozen \
      --extra ml \
      --group build
  fi

  mkdir -p "$PYINSTALLER_ROOT/dist" "$PYINSTALLER_ROOT/work"
  LOCAL_TRANSCRIPT_BUILD_FFMPEG_BIN="$(dirname "$FFMPEG")" \
    "$UV" run \
      --project "$WORKER_ROOT" \
      --frozen \
      --extra ml \
      --group build \
      pyinstaller \
      --noconfirm \
      --clean \
      --distpath "$PYINSTALLER_ROOT/dist" \
      --workpath "$PYINSTALLER_ROOT/work" \
      "$WORKER_ROOT/local_transcript_worker.spec"
fi

if [[ ! -x "$WORKER_BUNDLE/local-transcript-worker" ]]; then
  echo "PyInstaller worker bundle is missing: $WORKER_BUNDLE" >&2
  exit 1
fi
if ! file "$WORKER_BUNDLE/local-transcript-worker" | grep -q 'arm64'; then
  echo "The packaged worker is not an arm64 Mach-O executable." >&2
  exit 1
fi

mkdir -p "$RUNTIME_PARENT"
STAGING_ROOT="$(mktemp -d "$RUNTIME_PARENT/.runtime-stage.XXXXXX")"
trap 'rm -rf -- "$STAGING_ROOT"' EXIT
STAGING_RUNTIME="$STAGING_ROOT/runtime"
mkdir -p "$STAGING_RUNTIME"

# ditto preserves PyInstaller's relative dylib symlinks and executable modes.
/usr/bin/ditto "$WORKER_BUNDLE" "$STAGING_RUNTIME"
/bin/cp -L "$FFMPEG" "$STAGING_RUNTIME/ffmpeg"
/bin/cp -L "$FFPROBE" "$STAGING_RUNTIME/ffprobe"
/bin/chmod 0755 \
  "$STAGING_RUNTIME/local-transcript-worker" \
  "$STAGING_RUNTIME/ffmpeg" \
  "$STAGING_RUNTIME/ffprobe"

if [[ -x "$WORKER_ROOT/.venv/bin/python" ]]; then
  MANIFEST_PYTHON="$WORKER_ROOT/.venv/bin/python"
else
  MANIFEST_PYTHON="$(find_executable "" python3)"
fi
APP_VERSION="$($MANIFEST_PYTHON -c \
  'import json, pathlib, sys; print(json.loads(pathlib.Path(sys.argv[1]).read_text())["version"])' \
  "$REPOSITORY_ROOT/package.json")"
SOURCE_REVISION="$(git -C "$REPOSITORY_ROOT" rev-parse HEAD 2>/dev/null || printf 'unknown')"

"$MANIFEST_PYTHON" "$MANIFEST_HELPER" \
  --runtime-root "$STAGING_RUNTIME" \
  --model-manifest "$MODEL_MANIFEST" \
  --app-version "$APP_VERSION" \
  --source-revision "$SOURCE_REVISION" \
  --ffmpeg-version-line "$FFMPEG_VERSION_LINE" \
  --output "$STAGING_RUNTIME/runtime-manifest.json"

# This is the only destructive replacement in the script, and the target is a
# fixed repository-local build product rather than a caller-provided path.
rm -rf -- "$RUNTIME_OUTPUT"
mv "$STAGING_RUNTIME" "$RUNTIME_OUTPUT"

echo "Staged Apple Silicon runtime: $RUNTIME_OUTPUT"
du -sh "$RUNTIME_OUTPUT"
