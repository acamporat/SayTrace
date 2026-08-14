#!/bin/bash
set -euo pipefail

usage() {
  /bin/cat <<'EOF'
Sign the staged SayTrace runtime and regenerate its post-signing manifest.

Usage: script/sign_macos_runtime.sh [--development] [--runtime PATH]
       [--model-root PATH --require-model-inference]

Official mode requires a Developer ID Application identity and a secure
timestamp. --development permits Apple Development signing for local release-
candidate testing only; such output must never be published as an official
release.

Environment:
  SAYTRACE_CODESIGN_IDENTITY  certificate SHA-1 or full common name
  SAYTRACE_SOURCE_REVISION    manifest source revision (defaults to git HEAD)
EOF
}

DEVELOPMENT=0
MODEL_ROOT=""
REQUIRE_MODEL_INFERENCE=0
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
APPROVED_RUNTIME_PARENT="$REPOSITORY_ROOT/build/macos-runtime"
RUNTIME_ROOT="$REPOSITORY_ROOT/build/macos-runtime/runtime"
WORKER_ENTITLEMENTS="$REPOSITORY_ROOT/src-tauri/WorkerEntitlements.plist"
EXPECTED_TEAM_ID="ZQF63BSBBN"

verify_expected_team() {
  local signed_path="$1"
  local signature_details
  local team_identifier
  if ! signature_details="$(/usr/bin/codesign --display --verbose=4 "$signed_path" 2>&1)"; then
    echo "Unable to inspect the code signature: $signed_path" >&2
    return 1
  fi
  team_identifier="$(/usr/bin/sed -n 's/^TeamIdentifier=//p' <<<"$signature_details" | /usr/bin/head -n 1)"
  if [[ "$team_identifier" != "$EXPECTED_TEAM_ID" ]]; then
    echo "Code signature TeamIdentifier must be $EXPECTED_TEAM_ID: $signed_path" >&2
    return 1
  fi
}

while (($#)); do
  case "$1" in
    --development)
      DEVELOPMENT=1
      ;;
    --runtime)
      shift
      if (($# == 0)); then
        echo "--runtime requires a path." >&2
        exit 2
      fi
      RUNTIME_ROOT="$1"
      ;;
    --model-root)
      shift
      if (($# == 0)); then
        echo "--model-root requires a path." >&2
        exit 2
      fi
      MODEL_ROOT="$1"
      ;;
    --require-model-inference)
      REQUIRE_MODEL_INFERENCE=1
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

if ((DEVELOPMENT == 0 && REQUIRE_MODEL_INFERENCE == 0)); then
  echo "Official runtime signing requires packaged model inference." >&2
  exit 1
fi
if ((REQUIRE_MODEL_INFERENCE)); then
  if [[ -z "$MODEL_ROOT" || -L "$MODEL_ROOT" ]]; then
    echo "Packaged inference requires an ordinary model-root directory." >&2
    exit 1
  fi
  MODEL_ROOT="$(cd "$MODEL_ROOT" 2>/dev/null && pwd -P)" || {
    echo "Packaged inference model root is unavailable: $MODEL_ROOT" >&2
    exit 1
  }
fi

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "Runtime signing must run on Apple Silicon macOS." >&2
  exit 1
fi
if [[ -L "$RUNTIME_ROOT" ]]; then
  echo "Staged runtime root must not be a symbolic link: $RUNTIME_ROOT" >&2
  exit 1
fi
if [[ -L "$REPOSITORY_ROOT/build" || -L "$APPROVED_RUNTIME_PARENT" ]]; then
  echo "Approved runtime build path must not contain symbolic links." >&2
  exit 1
fi
RUNTIME_ROOT="$(cd "$RUNTIME_ROOT" 2>/dev/null && pwd -P)" || {
  echo "Staged runtime is unavailable: $RUNTIME_ROOT" >&2
  exit 1
}
APPROVED_RUNTIME_PARENT="$(cd "$APPROVED_RUNTIME_PARENT" 2>/dev/null && pwd -P)" || {
  echo "Approved runtime build directory is unavailable: $APPROVED_RUNTIME_PARENT" >&2
  exit 1
}
case "$RUNTIME_ROOT" in
  "$APPROVED_RUNTIME_PARENT"/*)
    ;;
  *)
    echo "Runtime signing is restricted to $APPROVED_RUNTIME_PARENT." >&2
    exit 1
    ;;
esac
for entrypoint in local-transcript-worker ffmpeg ffprobe; do
  entrypoint_path="$RUNTIME_ROOT/$entrypoint"
  if [[ -L "$entrypoint_path" ]]; then
    echo "Staged runtime entrypoint must not be a symbolic link: $entrypoint" >&2
    exit 1
  fi
  if [[ ! -f "$entrypoint_path" || ! -x "$entrypoint_path" ]]; then
    echo "Staged runtime entrypoint is missing: $entrypoint" >&2
    exit 1
  fi
done
if [[ -L "$WORKER_ENTITLEMENTS" || ! -f "$WORKER_ENTITLEMENTS" ]]; then
  echo "The packaged-worker hardened-runtime entitlements are missing." >&2
  exit 1
fi

IDENTITIES="$(/usr/bin/security find-identity -v -p codesigning 2>/dev/null || true)"
if [[ -n "${SAYTRACE_CODESIGN_IDENTITY:-}" ]]; then
  IDENTITY="$SAYTRACE_CODESIGN_IDENTITY"
else
  if ((DEVELOPMENT)); then
    IDENTITY="$(awk '/"Apple Development:/ { print $2; exit }' <<<"$IDENTITIES")"
  else
    IDENTITY="$(awk '/"Developer ID Application:/ { print $2; exit }' <<<"$IDENTITIES")"
  fi
fi
if [[ -z "$IDENTITY" ]]; then
  if ((DEVELOPMENT)); then
    echo "No Apple Development signing identity is installed." >&2
  else
    echo "No Developer ID Application signing identity is installed." >&2
  fi
  exit 1
fi
IDENTITY_RECORD="$(grep -F "$IDENTITY" <<<"$IDENTITIES" | head -1 || true)"
if [[ -z "$IDENTITY_RECORD" ]]; then
  echo "The requested signing identity is not available in the login Keychain." >&2
  exit 1
fi
IDENTITY_NAME="$(sed -E 's/^[^"]*"([^"]+)".*$/\1/' <<<"$IDENTITY_RECORD")"
if ((DEVELOPMENT)); then
  if [[ "$IDENTITY_NAME" != Apple\ Development:* ]]; then
    echo "--development requires an Apple Development identity." >&2
    exit 1
  fi
  TIMESTAMP_ARGUMENT="--timestamp=none"
else
  if [[ "$IDENTITY_NAME" != Developer\ ID\ Application:* ]]; then
    echo "Official release signing requires a Developer ID Application identity." >&2
    exit 1
  fi
  TIMESTAMP_ARGUMENT="--timestamp"
fi

SIGNABLES="$(mktemp "$REPOSITORY_ROOT/build/.runtime-signables.XXXXXX")"
BUNDLES="$(mktemp "$REPOSITORY_ROOT/build/.runtime-bundles.XXXXXX")"
cleanup_signing_inventories() {
  local signing_exit_code=$?
  set +e
  rm -f -- "$SIGNABLES" "$BUNDLES"
  return "$signing_exit_code"
}
trap cleanup_signing_inventories EXIT

# Sign every Mach-O leaf before signing any enclosing framework or bundle.
/usr/bin/find "$RUNTIME_ROOT" -type f -print | while IFS= read -r candidate; do
  if /usr/bin/file -b "$candidate" | grep -q 'Mach-O'; then
    /usr/bin/printf '%s\n' "$candidate"
  fi
done | LC_ALL=C /usr/bin/sort >"$SIGNABLES"

while IFS= read -r candidate; do
  [[ -n "$candidate" ]] || continue
  if [[ "$candidate" == "$RUNTIME_ROOT/local-transcript-worker" ]]; then
    /usr/bin/codesign \
      --force \
      --sign "$IDENTITY" \
      "$TIMESTAMP_ARGUMENT" \
      --options runtime \
      --entitlements "$WORKER_ENTITLEMENTS" \
      "$candidate"
  else
    /usr/bin/codesign \
      --force \
      --sign "$IDENTITY" \
      "$TIMESTAMP_ARGUMENT" \
      --options runtime \
      "$candidate"
  fi
  verify_expected_team "$candidate"
done <"$SIGNABLES"

# A materialized Python.framework no longer has the conventional directory
# symlinks that make the framework root signable as one bundle. In that case,
# sign each concrete framework version. Every executable alias then resolves to
# a signed Mach-O, while codesign still seals the version's Info.plist.
/usr/bin/find "$RUNTIME_ROOT" -type d \( -name '*.framework' -o -name '*.bundle' -o -name '*.xpc' \) -print \
  | while IFS= read -r candidate; do
    if [[ "$candidate" == *.framework && -d "$candidate/Versions" && ! -L "$candidate/Versions/Current" ]]; then
      found_version=0
      while IFS= read -r version_directory; do
        [[ -f "$version_directory/Resources/Info.plist" ]] || continue
        /usr/bin/printf '%s\n' "$version_directory"
        found_version=1
      done < <(/usr/bin/find "$candidate/Versions" -mindepth 1 -maxdepth 1 -type d -print)
      if ((found_version == 0)); then
        echo "Materialized framework has no signable version: $candidate" >&2
        exit 1
      fi
    else
      /usr/bin/printf '%s\n' "$candidate"
    fi
  done \
  | awk '{ print length($0) "\t" $0 }' \
  | LC_ALL=C /usr/bin/sort -rn \
  | cut -f2- >"$BUNDLES"
while IFS= read -r candidate; do
  [[ -n "$candidate" ]] || continue
  /usr/bin/codesign \
    --force \
    --sign "$IDENTITY" \
    "$TIMESTAMP_ARGUMENT" \
    --options runtime \
    "$candidate"
  verify_expected_team "$candidate"
done <"$BUNDLES"

while IFS= read -r candidate; do
  [[ -n "$candidate" ]] || continue
  /usr/bin/codesign --verify --strict "$candidate"
done <"$SIGNABLES"
while IFS= read -r candidate; do
  [[ -n "$candidate" ]] || continue
  /usr/bin/codesign --verify --strict "$candidate"
done <"$BUNDLES"

WORKER_SIGNATURE="$(/usr/bin/codesign --display --entitlements :- "$RUNTIME_ROOT/local-transcript-worker" 2>&1)"
grep -Fq '<key>com.apple.security.cs.allow-unsigned-executable-memory</key>' <<<"$WORKER_SIGNATURE" || {
  echo "The packaged worker is missing its required LLVM executable-memory entitlement." >&2
  exit 1
}

MODEL_INFERENCE="not_performed_by_packager"
if ((REQUIRE_MODEL_INFERENCE)); then
  "$REPOSITORY_ROOT/script/verify_packaged_worker.py" \
    --worker "$RUNTIME_ROOT/local-transcript-worker" \
    --ffmpeg "$RUNTIME_ROOT/ffmpeg" \
    --model-root "$MODEL_ROOT" \
    --require-model-inference
  MODEL_INFERENCE="passed"
else
  "$REPOSITORY_ROOT/script/verify_packaged_worker.py" \
    --worker "$RUNTIME_ROOT/local-transcript-worker" \
    --ffmpeg "$RUNTIME_ROOT/ffmpeg"
fi

PYTHON="$REPOSITORY_ROOT/worker/.venv/bin/python"
if [[ ! -x "$PYTHON" ]]; then
  PYTHON="$(command -v python3)"
fi
APP_VERSION="$("$PYTHON" -c 'import json, pathlib, sys; print(json.loads(pathlib.Path(sys.argv[1]).read_text())["version"])' "$REPOSITORY_ROOT/package.json")"
SOURCE_REVISION="${SAYTRACE_SOURCE_REVISION:-$(git -C "$REPOSITORY_ROOT" rev-parse HEAD)}"
FFMPEG_VERSION_LINE="$("$RUNTIME_ROOT/ffmpeg" -version | head -1)"

"$PYTHON" "$REPOSITORY_ROOT/script/generate_macos_runtime_manifest.py" \
  --runtime-root "$RUNTIME_ROOT" \
  --model-manifest "$REPOSITORY_ROOT/worker/model-manifest.macos.json" \
  --app-version "$APP_VERSION" \
  --source-revision "$SOURCE_REVISION" \
  --ffmpeg-version-line "$FFMPEG_VERSION_LINE" \
  --component-codesigned \
  --worker-handshake passed \
  --model-inference "$MODEL_INFERENCE" \
  --output "$RUNTIME_ROOT/runtime-manifest.json"

"$PYTHON" -c \
  'import json, pathlib, sys; data=json.loads(pathlib.Path(sys.argv[1]).read_text()); assert data["component_codesigned"] is True; assert data["runtime_validation"]["worker_handshake"] == "passed"; assert data["runtime_validation"]["model_inference"] == sys.argv[2]' \
  "$RUNTIME_ROOT/runtime-manifest.json" \
  "$MODEL_INFERENCE"

echo "Signed staged runtime and regenerated its integrity manifest with $IDENTITY_NAME."
