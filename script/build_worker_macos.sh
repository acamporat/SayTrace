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
  SAYTRACE_MACOS_WORKER_VENV    release venv (defaults to worker/.venv-macos)

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
REPOSITORY_BUILD_ROOT="$REPOSITORY_ROOT/build"
WORKER_ROOT="$REPOSITORY_ROOT/worker"
MACOS_WORKER_VENV="${SAYTRACE_MACOS_WORKER_VENV:-$WORKER_ROOT/.venv-macos}"
PYINSTALLER_ROOT="$REPOSITORY_ROOT/build/pyinstaller-macos"
WORKER_BUNDLE="$PYINSTALLER_ROOT/dist/local-transcript-worker"
RUNTIME_PARENT="$REPOSITORY_ROOT/build/macos-runtime"
RUNTIME_OUTPUT="$RUNTIME_PARENT/runtime"
MODEL_MANIFEST="$WORKER_ROOT/model-manifest.macos.json"
MANIFEST_HELPER="$SCRIPT_DIR/generate_macos_runtime_manifest.py"
MATERIALIZE_LINKS_HELPER="$SCRIPT_DIR/materialize_macos_runtime_directory_links.py"
PYINSTALLER_TEMP_ROOT=""
STAGING_ROOT=""

if [[ -L "$MACOS_WORKER_VENV" ]]; then
  echo "The macOS release worker environment must not be a symbolic link." >&2
  exit 1
fi
for directory in "$REPOSITORY_BUILD_ROOT" "$PYINSTALLER_ROOT" "$RUNTIME_PARENT"; do
  if [[ -L "$directory" || (-e "$directory" && ! -d "$directory") ]]; then
    echo "Worker build path must be an ordinary directory: $directory" >&2
    exit 1
  fi
  /bin/mkdir -p "$directory"
  resolved_directory="$(cd "$directory" && pwd -P)"
  case "$resolved_directory" in
    "$REPOSITORY_BUILD_ROOT" | "$REPOSITORY_BUILD_ROOT"/*)
      ;;
    *)
      echo "Worker build path resolves outside the repository build directory: $directory" >&2
      exit 1
      ;;
  esac
done
for directory in \
  "$PYINSTALLER_ROOT/dist" \
  "$WORKER_BUNDLE" \
  "$RUNTIME_OUTPUT"; do
  if [[ -L "$directory" || (-e "$directory" && ! -d "$directory") ]]; then
    echo "Worker output path must be an ordinary directory: $directory" >&2
    exit 1
  fi
done

cleanup_worker_stages() {
  local worker_exit_code=$?
  set +e
  if [[ -n "$PYINSTALLER_TEMP_ROOT" && "$PYINSTALLER_TEMP_ROOT" == "$REPOSITORY_BUILD_ROOT/.pyinstaller-macos."* ]]; then
    rm -rf -- "$PYINSTALLER_TEMP_ROOT"
  fi
  if [[ -n "$STAGING_ROOT" && "$STAGING_ROOT" == "$RUNTIME_PARENT/.runtime-stage."* ]]; then
    rm -rf -- "$STAGING_ROOT"
  fi
  return "$worker_exit_code"
}
trap cleanup_worker_stages EXIT

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
  | awk '$0 ~ /^[[:space:]]+\// { print $1 }' \
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
    MACOSX_DEPLOYMENT_TARGET=15.0 \
      UV_PROJECT_ENVIRONMENT="$MACOS_WORKER_VENV" \
      "$UV" sync \
      --project "$WORKER_ROOT" \
      --frozen \
      --extra ml \
      --group build \
      --no-dev \
      --managed-python \
      --python 3.13.12 \
      --python-platform aarch64-apple-darwin
  fi

  PYINSTALLER="$MACOS_WORKER_VENV/bin/pyinstaller"
  if [[ ! -x "$PYINSTALLER" ]]; then
    echo "The locked macOS release environment is missing PyInstaller: $MACOS_WORKER_VENV" >&2
    exit 1
  fi
  PYINSTALLER_TEMP_ROOT="$(mktemp -d "$REPOSITORY_BUILD_ROOT/.pyinstaller-macos.XXXXXX")"
  TEMP_DIST_ROOT="$PYINSTALLER_TEMP_ROOT/dist"
  TEMP_WORK_ROOT="$PYINSTALLER_TEMP_ROOT/work"
  TEMP_WORKER_BUNDLE="$TEMP_DIST_ROOT/local-transcript-worker"
  mkdir -p "$TEMP_DIST_ROOT" "$TEMP_WORK_ROOT"
  MACOSX_DEPLOYMENT_TARGET=15.0 \
    LOCAL_TRANSCRIPT_BUILD_FFMPEG_BIN="$(dirname "$FFMPEG")" \
    "$PYINSTALLER" \
      --noconfirm \
      --clean \
      --distpath "$TEMP_DIST_ROOT" \
      --workpath "$TEMP_WORK_ROOT" \
      "$WORKER_ROOT/local_transcript_worker.spec"

  if [[ ! -x "$TEMP_WORKER_BUNDLE/local-transcript-worker" ]]; then
    echo "Fresh PyInstaller worker bundle is missing: $TEMP_WORKER_BUNDLE" >&2
    exit 1
  fi
  for directory in "$REPOSITORY_BUILD_ROOT" "$PYINSTALLER_ROOT"; do
    if [[ -L "$directory" || ! -d "$directory" ]]; then
      echo "Worker build path became unsafe before bundle promotion: $directory" >&2
      exit 1
    fi
    resolved_directory="$(cd "$directory" && pwd -P)"
    case "$resolved_directory" in
      "$REPOSITORY_BUILD_ROOT" | "$REPOSITORY_BUILD_ROOT"/*)
        ;;
      *)
        echo "Worker build path resolves outside the repository build directory: $directory" >&2
        exit 1
        ;;
    esac
  done
  if [[ -L "$PYINSTALLER_ROOT/dist" || (-e "$PYINSTALLER_ROOT/dist" && ! -d "$PYINSTALLER_ROOT/dist") ]]; then
    echo "Worker bundle cache became unsafe before promotion." >&2
    exit 1
  fi
  mkdir -p "$PYINSTALLER_ROOT/dist"
  if [[ -L "$WORKER_BUNDLE" || (-e "$WORKER_BUNDLE" && ! -d "$WORKER_BUNDLE") ]]; then
    echo "Worker bundle output became unsafe before promotion: $WORKER_BUNDLE" >&2
    exit 1
  fi
  rm -rf -- "$WORKER_BUNDLE"
  mv "$TEMP_WORKER_BUNDLE" "$WORKER_BUNDLE"
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
STAGING_RUNTIME="$STAGING_ROOT/runtime"
mkdir -p "$STAGING_RUNTIME"

if [[ -x "$MACOS_WORKER_VENV/bin/python" ]]; then
  MANIFEST_PYTHON="$MACOS_WORKER_VENV/bin/python"
elif [[ -x "$WORKER_ROOT/.venv/bin/python" ]]; then
  MANIFEST_PYTHON="$WORKER_ROOT/.venv/bin/python"
else
  MANIFEST_PYTHON="$(find_executable "" python3)"
fi

# ditto preserves PyInstaller's relative dylib symlinks and executable modes.
/usr/bin/ditto "$WORKER_BUNDLE" "$STAGING_RUNTIME"
"$MANIFEST_PYTHON" "$MATERIALIZE_LINKS_HELPER" --runtime "$STAGING_RUNTIME"
/bin/cp -L "$FFMPEG" "$STAGING_RUNTIME/ffmpeg"
/bin/cp -L "$FFPROBE" "$STAGING_RUNTIME/ffprobe"
/bin/chmod 0755 \
  "$STAGING_RUNTIME/local-transcript-worker" \
  "$STAGING_RUNTIME/ffmpeg" \
  "$STAGING_RUNTIME/ffprobe"

APP_VERSION="$("$MANIFEST_PYTHON" -c \
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
for directory in "$REPOSITORY_BUILD_ROOT" "$RUNTIME_PARENT"; do
  if [[ -L "$directory" || ! -d "$directory" ]]; then
    echo "Worker build path became unsafe before runtime promotion: $directory" >&2
    exit 1
  fi
  resolved_directory="$(cd "$directory" && pwd -P)"
  case "$resolved_directory" in
    "$REPOSITORY_BUILD_ROOT" | "$REPOSITORY_BUILD_ROOT"/*)
      ;;
    *)
      echo "Worker build path resolves outside the repository build directory: $directory" >&2
      exit 1
      ;;
  esac
done
if [[ -L "$RUNTIME_OUTPUT" || (-e "$RUNTIME_OUTPUT" && ! -d "$RUNTIME_OUTPUT") ]]; then
  echo "Worker runtime output became unsafe before promotion: $RUNTIME_OUTPUT" >&2
  exit 1
fi
resolved_staging_root="$(cd "$STAGING_ROOT" && pwd -P)"
case "$resolved_staging_root" in
  "$RUNTIME_PARENT"/.runtime-stage.*)
    ;;
  *)
    echo "Worker runtime stage escaped its approved parent." >&2
    exit 1
    ;;
esac
rm -rf -- "$RUNTIME_OUTPUT"
mv "$STAGING_RUNTIME" "$RUNTIME_OUTPUT"

echo "Staged Apple Silicon runtime: $RUNTIME_OUTPUT"
du -sh "$RUNTIME_OUTPUT"
