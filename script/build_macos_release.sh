#!/bin/bash
set -euo pipefail

usage() {
  /bin/cat <<'EOF'
Build, sign, verify, notarize, and package SayTrace for Apple Silicon macOS.

Usage: script/build_macos_release.sh [--candidate] [--skip-ffmpeg-build] [--skip-worker-build]

Official preparation is the default. It fails closed unless a Developer ID
Application identity and a valid notarytool Keychain profile are available,
then creates an immutable notarized DMG under artifacts/macos/prepared for
physical testing. finalize_macos_release.sh promotes only those exact accepted
bytes. --candidate uses Apple Development signing and creates an explicitly
UNNOTARIZED artifact for local integration testing; candidate output must not
be published as an official release.

Environment:
  SAYTRACE_CODESIGN_IDENTITY  certificate SHA-1 or full common name
  SAYTRACE_NOTARY_PROFILE     notarytool Keychain profile (official mode)
  SAYTRACE_RELEASE_MODEL_ROOT verified first-run model directory
  SAYTRACE_FFMPEG_VERSION     source-built FFmpeg version (default: 8.1.2)
EOF
}

CANDIDATE=0
SKIP_FFMPEG=0
SKIP_WORKER=0
while (($#)); do
  case "$1" in
    --candidate)
      CANDIDATE=1
      ;;
    --skip-ffmpeg-build)
      SKIP_FFMPEG=1
      ;;
    --skip-worker-build)
      SKIP_WORKER=1
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
  echo "The macOS release must be built natively on Apple Silicon." >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
EXPECTED_TEAM_ID="ZQF63BSBBN"
EXPECTED_NODE_MAJOR="22"
EXPECTED_NPM_VERSION="10.9.8"
REPOSITORY_BUILD_ROOT="$REPOSITORY_ROOT/build"
REPOSITORY_ARTIFACT_ROOT="$REPOSITORY_ROOT/artifacts"
CURRENT_USER="$(/usr/bin/id -un)"

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

assert_private_release_directory() {
  local directory="$1"
  local directory_mode
  local directory_owner
  directory_owner="$(/usr/bin/stat -f '%Su' "$directory")"
  directory_mode="$(/usr/bin/stat -f '%Lp' "$directory")"
  if [[ "$directory_owner" != "$CURRENT_USER" ]]; then
    echo "Release directories must be owned by $CURRENT_USER: $directory" >&2
    return 1
  fi
  if ((8#$directory_mode & 8#22)); then
    echo "Release directories must not be group- or world-writable: $directory" >&2
    return 1
  fi
}

ensure_output_directory() {
  local directory="$1"
  local allowed_root="$2"
  local resolved_allowed_root
  local resolved_directory
  if [[ -L "$allowed_root" || ! -d "$allowed_root" ]]; then
    echo "Release output root must be an ordinary directory: $allowed_root" >&2
    return 1
  fi
  assert_private_release_directory "$allowed_root"
  if [[ -L "$directory" || (-e "$directory" && ! -d "$directory") ]]; then
    echo "Release output path must be an ordinary directory: $directory" >&2
    return 1
  fi
  /bin/mkdir -p "$directory"
  assert_private_release_directory "$directory"
  resolved_directory="$(cd "$directory" && pwd -P)"
  resolved_allowed_root="$(cd "$allowed_root" && pwd -P)"
  case "$resolved_directory" in
    "$resolved_allowed_root" | "$resolved_allowed_root"/*)
      ;;
    *)
      echo "Release output path resolves outside its approved root: $directory" >&2
      return 1
      ;;
  esac
}

assert_optional_output_directory() {
  local directory="$1"
  local allowed_root="$2"
  if [[ -e "$directory" || -L "$directory" ]]; then
    ensure_output_directory "$directory" "$allowed_root"
  fi
}

for directory in "$REPOSITORY_BUILD_ROOT" "$REPOSITORY_ARTIFACT_ROOT"; do
  ensure_output_directory "$directory" "$REPOSITORY_ROOT"
done
PYTHON="$REPOSITORY_ROOT/worker/.venv/bin/python"
WORKER_RELEASE_PYTHON="$REPOSITORY_ROOT/worker/.venv-macos/bin/python"
if [[ ! -x "$PYTHON" ]]; then
  echo "The locked worker environment is missing; run uv sync first." >&2
  exit 1
fi
VERSION="$("$PYTHON" -c 'import json, pathlib, sys; print(json.loads(pathlib.Path(sys.argv[1]).read_text())["version"])' "$REPOSITORY_ROOT/package.json")"
if [[ ! "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "macOS release versions must be stable semantic versions: $VERSION" >&2
  exit 1
fi
SOURCE_REVISION="$(git -C "$REPOSITORY_ROOT" rev-parse HEAD)"
if ((CANDIDATE)); then
  SOURCE_REVISION="$SOURCE_REVISION-dirty-candidate"
fi
FFMPEG_VERSION="${SAYTRACE_FFMPEG_VERSION:-8.1.2}"
FFMPEG_ROOT="$REPOSITORY_ROOT/build/ffmpeg-macos-arm64/$FFMPEG_VERSION"
FFMPEG_PREFIX="$FFMPEG_ROOT/prefix"
FFMPEG="$FFMPEG_PREFIX/bin/ffmpeg"
FFPROBE="$FFMPEG_PREFIX/bin/ffprobe"
FFMPEG_SOURCE="$FFMPEG_ROOT/downloads/ffmpeg-$FFMPEG_VERSION.tar.xz"
FFMPEG_SIGNATURE="$FFMPEG_SOURCE.asc"
FFMPEG_SOURCE_URL="https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz"
RUNTIME_ROOT="$REPOSITORY_ROOT/build/macos-runtime/runtime"
LEGAL_ROOT="$REPOSITORY_ROOT/build/release-resources/legal"
VITE_DIST_ROOT="$REPOSITORY_BUILD_ROOT/vite-macos-release"
TAURI_TARGET_ROOT=""
APP_BUNDLE=""
APP_BINARY=""
MACOS_ARTIFACT_ROOT="$REPOSITORY_ARTIFACT_ROOT/macos"
if ((CANDIDATE)); then
  CANDIDATE_PARENT="$MACOS_ARTIFACT_ROOT/candidates"
  FINAL_CANDIDATE_ROOT="$CANDIDATE_PARENT/v$VERSION"
  ARTIFACT_ROOT=""
else
  PREPARED_PARENT="$MACOS_ARTIFACT_ROOT/prepared"
  FINAL_PREPARED_ROOT="$PREPARED_PARENT/v$VERSION"
  ARTIFACT_ROOT=""
fi
NOTARY_ROOT=""
DMG_STAGE=""
PREPARED_STAGE=""
CANDIDATE_STAGE=""
RELEASE_LOCK="$REPOSITORY_BUILD_ROOT/.saytrace-release.lock"
RELEASE_LOCK_ACQUIRED=0
MODEL_ROOT="${SAYTRACE_RELEASE_MODEL_ROOT:-$HOME/Library/Application Support/com.localtranscript.desktop/models}"

ensure_output_directory "$REPOSITORY_BUILD_ROOT/release-resources" "$REPOSITORY_BUILD_ROOT"
ensure_output_directory "$LEGAL_ROOT" "$REPOSITORY_BUILD_ROOT"
ensure_output_directory "$VITE_DIST_ROOT" "$REPOSITORY_BUILD_ROOT"
ensure_output_directory "$MACOS_ARTIFACT_ROOT" "$REPOSITORY_ARTIFACT_ROOT"
if ((CANDIDATE)); then
  ensure_output_directory "$CANDIDATE_PARENT" "$MACOS_ARTIFACT_ROOT"
else
  ensure_output_directory "$PREPARED_PARENT" "$MACOS_ARTIFACT_ROOT"
fi
if [[ -L "$REPOSITORY_ROOT/node_modules" || (-e "$REPOSITORY_ROOT/node_modules" && ! -d "$REPOSITORY_ROOT/node_modules") ]]; then
  echo "Release dependency path must be an ordinary directory: node_modules" >&2
  exit 1
fi

for command_name in cargo codesign hdiutil node npm spctl xcrun; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "Missing required command: $command_name" >&2
    exit 1
  fi
done
NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
NPM_VERSION="$(npm --version)"
if [[ "$NODE_MAJOR" != "$EXPECTED_NODE_MAJOR" ]]; then
  echo "macOS releases require Node.js $EXPECTED_NODE_MAJOR.x; found $(node --version)." >&2
  exit 1
fi
if [[ "$NPM_VERSION" != "$EXPECTED_NPM_VERSION" ]]; then
  echo "macOS releases require npm $EXPECTED_NPM_VERSION; found $NPM_VERSION." >&2
  exit 1
fi

VERSIONS="$("$PYTHON" - "$REPOSITORY_ROOT" <<'PY'
import json
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
values = {
    "package": json.loads((root / "package.json").read_text())["version"],
    "package_lock": json.loads((root / "package-lock.json").read_text())["version"],
    "tauri": json.loads((root / "src-tauri/tauri.conf.json").read_text())["version"],
}
cargo = (root / "src-tauri/Cargo.toml").read_text()
match = re.search(r'^version\s*=\s*"([^"]+)"', cargo, re.MULTILINE)
if not match:
    raise SystemExit("Cargo package version is missing")
values["cargo"] = match.group(1)
print("\n".join(sorted(set(values.values()))))
PY
)"
if [[ "$VERSIONS" == *$'\n'* || "$VERSIONS" != "$VERSION" ]]; then
  echo "Release version metadata is inconsistent: $VERSIONS" >&2
  exit 1
fi

assert_source_clean() {
  local status
  status="$(git -C "$REPOSITORY_ROOT" status --porcelain=v1 --untracked-files=all)"
  if [[ -n "$status" ]]; then
    echo "Official releases require a completely clean source worktree:" >&2
    echo "$status" >&2
    return 1
  fi
}

remove_owned_stage() {
  local stage="$1"
  local prefix="$2"
  [[ -n "$stage" ]] || return 0
  case "$stage" in
    "$prefix"*)
      rm -rf -- "$stage"
      ;;
    *)
      echo "Refusing to clean a release stage outside its approved parent: $stage" >&2
      return 1
      ;;
  esac
}

cleanup_release_stages() {
  local release_exit_code=$?
  set +e
  remove_owned_stage "${NOTARY_ROOT:-}" "$REPOSITORY_BUILD_ROOT/.notary-macos."
  remove_owned_stage "${DMG_STAGE:-}" "$REPOSITORY_BUILD_ROOT/.dmg-staging."
  remove_owned_stage "${TAURI_TARGET_ROOT:-}" "$REPOSITORY_BUILD_ROOT/.tauri-macos-target."
  if [[ -n "${PREPARED_PARENT:-}" ]]; then
    remove_owned_stage "${PREPARED_STAGE:-}" "$PREPARED_PARENT/.v$VERSION-prepare."
  fi
  if [[ -n "${CANDIDATE_PARENT:-}" ]]; then
    remove_owned_stage "${CANDIDATE_STAGE:-}" "$CANDIDATE_PARENT/.v$VERSION-candidate."
  fi
  if ((RELEASE_LOCK_ACQUIRED)) && [[ ! -L "$RELEASE_LOCK" && -d "$RELEASE_LOCK" ]]; then
    /bin/rmdir "$RELEASE_LOCK" || true
  fi
  return "$release_exit_code"
}
trap cleanup_release_stages EXIT

if [[ -e "$RELEASE_LOCK" || -L "$RELEASE_LOCK" ]]; then
  echo "Another macOS release build may be using this checkout: $RELEASE_LOCK" >&2
  exit 1
fi
if ! /bin/mkdir "$RELEASE_LOCK"; then
  echo "Could not acquire the exclusive macOS release lock: $RELEASE_LOCK" >&2
  exit 1
fi
RELEASE_LOCK_ACQUIRED=1

IDENTITIES="$(/usr/bin/security find-identity -v -p codesigning 2>/dev/null || true)"
if [[ -n "${SAYTRACE_CODESIGN_IDENTITY:-}" ]]; then
  IDENTITY="$SAYTRACE_CODESIGN_IDENTITY"
elif ((CANDIDATE)); then
  IDENTITY="$(awk '/"Apple Development:/ { print $2; exit }' <<<"$IDENTITIES")"
else
  IDENTITY="$(awk '/"Developer ID Application:/ { print $2; exit }' <<<"$IDENTITIES")"
fi
if [[ -z "$IDENTITY" ]]; then
  if ((CANDIDATE)); then
    echo "Candidate signing requires an Apple Development identity." >&2
  else
    echo "Official signing requires a Developer ID Application identity." >&2
  fi
  exit 1
fi
IDENTITY_RECORD="$(grep -F "$IDENTITY" <<<"$IDENTITIES" | head -1 || true)"
IDENTITY_NAME="$(sed -E 's/^[^"]*"([^"]+)".*$/\1/' <<<"$IDENTITY_RECORD")"
if ((CANDIDATE)); then
  [[ "$IDENTITY_NAME" == Apple\ Development:* ]] || {
    echo "Candidate signing requires an Apple Development identity." >&2
    exit 1
  }
  TIMESTAMP_ARGUMENT="--timestamp=none"
else
  [[ "$IDENTITY_NAME" == Developer\ ID\ Application:* ]] || {
    echo "Official signing requires a Developer ID Application identity." >&2
    exit 1
  }
  TIMESTAMP_ARGUMENT="--timestamp"
fi

if ((CANDIDATE == 0)); then
  if ((SKIP_FFMPEG || SKIP_WORKER)); then
    echo "Official releases cannot skip FFmpeg or worker builds." >&2
    exit 1
  fi
  assert_source_clean
  if git -C "$REPOSITORY_ROOT" rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
    echo "Release tag v$VERSION already exists." >&2
    exit 1
  fi
  if [[ -e "$FINAL_PREPARED_ROOT" || -L "$FINAL_PREPARED_ROOT" ]]; then
    echo "Prepared release output already exists and will not be overwritten: $FINAL_PREPARED_ROOT" >&2
    exit 1
  fi
  if [[ -z "${SAYTRACE_NOTARY_PROFILE:-}" ]]; then
    echo "SAYTRACE_NOTARY_PROFILE must name a notarytool Keychain profile." >&2
    exit 1
  fi
  if [[ ! -d "$MODEL_ROOT" ]]; then
    echo "Official packaged inference requires the verified model root: $MODEL_ROOT" >&2
    exit 1
  fi
  xcrun notarytool history \
    --keychain-profile "$SAYTRACE_NOTARY_PROFILE" \
    --output-format json >/dev/null

  ensure_output_directory "$PREPARED_PARENT" "$MACOS_ARTIFACT_ROOT"
  PREPARED_STAGE="$(mktemp -d "$PREPARED_PARENT/.v$VERSION-prepare.XXXXXX")"
  ARTIFACT_ROOT="$PREPARED_STAGE"
else
  ensure_output_directory "$CANDIDATE_PARENT" "$MACOS_ARTIFACT_ROOT"
  if [[ -e "$FINAL_CANDIDATE_ROOT" || -L "$FINAL_CANDIDATE_ROOT" ]]; then
    echo "Candidate release output already exists and will not be overwritten: $FINAL_CANDIDATE_ROOT" >&2
    exit 1
  fi
  CANDIDATE_STAGE="$(mktemp -d "$CANDIDATE_PARENT/.v$VERSION-candidate.XXXXXX")"
  ARTIFACT_ROOT="$CANDIDATE_STAGE"
fi

(
  cd "$REPOSITORY_ROOT"
  npm ci \
    --install-strategy=hoisted \
    --include=prod \
    --include=dev \
    --include=optional \
    --include=peer \
    --bin-links=true \
    --ignore-scripts=false \
    --no-audit \
    --no-fund
)

if ((SKIP_FFMPEG == 0)); then
  if ((CANDIDATE)); then
    "$REPOSITORY_ROOT/script/build_ffmpeg_macos.sh"
  else
    "$REPOSITORY_ROOT/script/build_ffmpeg_macos.sh" --rebuild
  fi
else
  "$REPOSITORY_ROOT/script/build_ffmpeg_macos.sh" --verify-only
fi
if [[ ! -x "$FFMPEG" || ! -x "$FFPROBE" || ! -f "$FFMPEG_SOURCE" || ! -f "$FFMPEG_SIGNATURE" ]]; then
  echo "Verified release FFmpeg inputs are missing." >&2
  exit 1
fi
"$REPOSITORY_ROOT/script/verify_ffmpeg_macos.sh" "$FFMPEG_PREFIX"

if ((SKIP_WORKER == 0)); then
  LOCAL_TRANSCRIPT_FFMPEG="$FFMPEG" \
    LOCAL_TRANSCRIPT_FFPROBE="$FFPROBE" \
    "$REPOSITORY_ROOT/script/build_worker_macos.sh"
fi
if [[ ! -d "$RUNTIME_ROOT" ]]; then
  echo "The staged macOS runtime is missing." >&2
  exit 1
fi
if [[ ! -x "$WORKER_RELEASE_PYTHON" ]]; then
  echo "The macOS 15-targeted worker environment is missing; rebuild the worker without --skip-worker-build." >&2
  exit 1
fi

if ((CANDIDATE)); then
  if [[ -d "$MODEL_ROOT" ]]; then
    SAYTRACE_SOURCE_REVISION="$SOURCE_REVISION" \
      SAYTRACE_CODESIGN_IDENTITY="$IDENTITY" \
      "$REPOSITORY_ROOT/script/sign_macos_runtime.sh" \
        --development \
        --model-root "$MODEL_ROOT" \
        --require-model-inference
  else
    SAYTRACE_SOURCE_REVISION="$SOURCE_REVISION" \
      SAYTRACE_CODESIGN_IDENTITY="$IDENTITY" \
      "$REPOSITORY_ROOT/script/sign_macos_runtime.sh" --development
  fi
else
  SAYTRACE_SOURCE_REVISION="$SOURCE_REVISION" \
    SAYTRACE_CODESIGN_IDENTITY="$IDENTITY" \
    "$REPOSITORY_ROOT/script/sign_macos_runtime.sh" \
      --model-root "$MODEL_ROOT" \
      --require-model-inference
fi

"$REPOSITORY_ROOT/script/verify_macos_runtime.py" \
  --runtime "$RUNTIME_ROOT" \
  --version "$VERSION" \
  --source-revision "$SOURCE_REVISION"

"$PYTHON" "$REPOSITORY_ROOT/script/generate_release_notices.py" \
  --repository-root "$REPOSITORY_ROOT" \
  --output "$LEGAL_ROOT" \
  --ffmpeg-source "$FFMPEG_SOURCE_URL" \
  --ffmpeg-prefix "$FFMPEG_PREFIX" \
  --python "$WORKER_RELEASE_PYTHON"

ensure_output_directory "$LEGAL_ROOT" "$REPOSITORY_BUILD_ROOT"
ensure_output_directory "$VITE_DIST_ROOT" "$REPOSITORY_BUILD_ROOT"
ensure_output_directory "$REPOSITORY_BUILD_ROOT" "$REPOSITORY_ROOT"
TAURI_TARGET_ROOT="$(mktemp -d "$REPOSITORY_BUILD_ROOT/.tauri-macos-target.XXXXXX")"
APP_BUNDLE="$TAURI_TARGET_ROOT/release/bundle/macos/SayTrace.app"
APP_BINARY="$APP_BUNDLE/Contents/MacOS/local-transcript"
if [[ -L "$REPOSITORY_ROOT/node_modules" || ! -d "$REPOSITORY_ROOT/node_modules" ]]; then
  echo "The locked release dependency directory became unsafe." >&2
  exit 1
fi

cd "$REPOSITORY_ROOT"
CARGO_TARGET_DIR="$TAURI_TARGET_ROOT" npm run tauri:build -- \
  --config src-tauri/tauri.macos.release.conf.json \
  --bundles app \
  --ci \
  --no-sign \
  -- \
  --locked

if ((CANDIDATE == 0)); then
  assert_source_clean
fi

ensure_output_directory "$TAURI_TARGET_ROOT" "$REPOSITORY_BUILD_ROOT"
ensure_output_directory "$TAURI_TARGET_ROOT/release" "$TAURI_TARGET_ROOT"
ensure_output_directory "$TAURI_TARGET_ROOT/release/bundle" "$TAURI_TARGET_ROOT"
ensure_output_directory "$TAURI_TARGET_ROOT/release/bundle/macos" "$TAURI_TARGET_ROOT"
ensure_output_directory "$APP_BUNDLE" "$TAURI_TARGET_ROOT"
if [[ ! -x "$APP_BINARY" || ! -d "$APP_BUNDLE/Contents/Resources/runtime" ]]; then
  echo "Tauri did not produce a self-contained SayTrace app bundle." >&2
  exit 1
fi

/usr/bin/codesign \
  --force \
  --sign "$IDENTITY" \
  "$TIMESTAMP_ARGUMENT" \
  --options runtime \
  --entitlements "$REPOSITORY_ROOT/src-tauri/Entitlements.plist" \
  "$APP_BUNDLE"
/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"
verify_expected_team "$APP_BUNDLE"
"$PYTHON" "$REPOSITORY_ROOT/script/verify_macos_macho.py" --path "$APP_BINARY"

"$REPOSITORY_ROOT/script/verify_macos_runtime.py" \
  --runtime "$APP_BUNDLE/Contents/Resources/runtime" \
  --version "$VERSION" \
  --source-revision "$SOURCE_REVISION"

ensure_output_directory "$REPOSITORY_BUILD_ROOT" "$REPOSITORY_ROOT"
NOTARY_ROOT="$(mktemp -d "$REPOSITORY_BUILD_ROOT/.notary-macos.XXXXXX")"
DMG_STAGE="$(mktemp -d "$REPOSITORY_BUILD_ROOT/.dmg-staging.XXXXXX")"

if ((CANDIDATE == 0)); then
  APP_ZIP="$NOTARY_ROOT/SayTrace-$VERSION-macos-arm64.zip"
  /usr/bin/ditto -c -k --keepParent "$APP_BUNDLE" "$APP_ZIP"
  APP_NOTARY_RESULT="$NOTARY_ROOT/app-notarization.json"
  xcrun notarytool submit "$APP_ZIP" \
    --keychain-profile "$SAYTRACE_NOTARY_PROFILE" \
    --wait \
    --output-format json >"$APP_NOTARY_RESULT"
  "$PYTHON" -c 'import json,sys; data=json.load(open(sys.argv[1])); assert data["status"] == "Accepted", data' "$APP_NOTARY_RESULT"
  xcrun stapler staple "$APP_BUNDLE"
  xcrun stapler validate "$APP_BUNDLE"
  /usr/sbin/spctl --assess --type execute --verbose=4 "$APP_BUNDLE"
fi

/usr/bin/ditto "$APP_BUNDLE" "$DMG_STAGE/SayTrace.app"
/bin/ln -s /Applications "$DMG_STAGE/Applications"
if ((CANDIDATE)); then
  DMG="$ARTIFACT_ROOT/SayTrace-$VERSION-macos-arm64-UNNOTARIZED.dmg"
else
  DMG="$ARTIFACT_ROOT/SayTrace-$VERSION-macos-arm64.dmg"
fi
/usr/bin/hdiutil create \
  -volname "SayTrace $VERSION" \
  -srcfolder "$DMG_STAGE" \
  -ov \
  -format UDZO \
  "$DMG"
/usr/bin/codesign --force --sign "$IDENTITY" "$TIMESTAMP_ARGUMENT" "$DMG"
/usr/bin/codesign --verify --strict --verbose=2 "$DMG"
verify_expected_team "$DMG"

if ((CANDIDATE == 0)); then
  DMG_NOTARY_RESULT="$NOTARY_ROOT/dmg-notarization.json"
  xcrun notarytool submit "$DMG" \
    --keychain-profile "$SAYTRACE_NOTARY_PROFILE" \
    --wait \
    --output-format json >"$DMG_NOTARY_RESULT"
  "$PYTHON" -c 'import json,sys; data=json.load(open(sys.argv[1])); assert data["status"] == "Accepted", data' "$DMG_NOTARY_RESULT"
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"
  /usr/sbin/spctl --assess --type open --context context:primary-signature --verbose=4 "$DMG"
fi

SOURCE_ASSET="$ARTIFACT_ROOT/SayTrace-$VERSION-ffmpeg-$FFMPEG_VERSION-source.tar.xz"
SOURCE_SIGNATURE_ASSET="$SOURCE_ASSET.asc"
/bin/cp "$FFMPEG_SOURCE" "$SOURCE_ASSET"
/bin/cp "$FFMPEG_SIGNATURE" "$SOURCE_SIGNATURE_ASSET"
/bin/cp "$REPOSITORY_ROOT/docs/releases/v$VERSION.md" "$ARTIFACT_ROOT/RELEASE_NOTES.md"

if ((CANDIDATE == 0)); then
  RELEASE_STATE="$ARTIFACT_ROOT/.release-state"
  /bin/mkdir -p "$RELEASE_STATE"
  PREPARED_APP_ZIP="$RELEASE_STATE/$(basename "$APP_ZIP")"
  PREPARED_APP_NOTARY_RESULT="$RELEASE_STATE/app-notarization.json"
  PREPARED_DMG_NOTARY_RESULT="$RELEASE_STATE/dmg-notarization.json"
  /bin/cp "$APP_ZIP" "$PREPARED_APP_ZIP"
  /bin/cp "$APP_NOTARY_RESULT" "$PREPARED_APP_NOTARY_RESULT"
  /bin/cp "$DMG_NOTARY_RESULT" "$PREPARED_DMG_NOTARY_RESULT"
  "$PYTHON" "$REPOSITORY_ROOT/script/generate_macos_prepared_release.py" \
    --version "$VERSION" \
    --source-revision "$SOURCE_REVISION" \
    --dmg "$DMG" \
    --app-notary-payload "$PREPARED_APP_ZIP" \
    --app-notary-result "$PREPARED_APP_NOTARY_RESULT" \
    --dmg-notary-result "$PREPARED_DMG_NOTARY_RESULT" \
    --runtime-manifest "$APP_BUNDLE/Contents/Resources/runtime/runtime-manifest.json" \
    --ffmpeg-source "$SOURCE_ASSET" \
    --ffmpeg-source-signature "$SOURCE_SIGNATURE_ASSET" \
    --release-notes "$ARTIFACT_ROOT/RELEASE_NOTES.md" \
    --output "$ARTIFACT_ROOT/prepared-release.json" \
    --acceptance-template-output "$ARTIFACT_ROOT/physical-acceptance-template.json"
  "$PYTHON" "$REPOSITORY_ROOT/script/verify_macos_prepared_release.py" \
    --prepared-root "$ARTIFACT_ROOT" \
    --version "$VERSION" \
    --source-revision "$SOURCE_REVISION" \
    --ffmpeg-version "$FFMPEG_VERSION" \
    --runtime-manifest "$APP_BUNDLE/Contents/Resources/runtime/runtime-manifest.json"
  ensure_output_directory "$PREPARED_PARENT" "$MACOS_ARTIFACT_ROOT"
  if [[ -e "$FINAL_PREPARED_ROOT" || -L "$FINAL_PREPARED_ROOT" ]]; then
    echo "Prepared release output appeared during the build and will not be overwritten: $FINAL_PREPARED_ROOT" >&2
    exit 1
  fi
  "$PYTHON" - "$ARTIFACT_ROOT" "$FINAL_PREPARED_ROOT" <<'PY'
import os
import sys

os.rename(sys.argv[1], sys.argv[2])
PY
  PREPARED_STAGE=""
  ARTIFACT_ROOT="$FINAL_PREPARED_ROOT"
  DMG="$ARTIFACT_ROOT/$(basename "$DMG")"
  echo "Prepared exact Developer ID signed, notarized, stapled installer: $DMG"
  echo "Complete physical-acceptance-template.json on the test Mac, then run script/finalize_macos_release.sh."
  du -sh "$APP_BUNDLE" "$DMG"
  exit 0
fi

MANIFEST="$ARTIFACT_ROOT/SayTrace-$VERSION-macos-arm64.release-manifest.json"
"$PYTHON" "$REPOSITORY_ROOT/script/generate_macos_release_manifest.py" \
  --version "$VERSION" \
  --source-revision "$SOURCE_REVISION" \
  --dmg "$DMG" \
  --runtime-manifest "$APP_BUNDLE/Contents/Resources/runtime/runtime-manifest.json" \
  --ffmpeg-source "$SOURCE_ASSET" \
  --ffmpeg-source-signature "$SOURCE_SIGNATURE_ASSET" \
  --output "$MANIFEST"

(
  cd "$ARTIFACT_ROOT"
  /usr/bin/shasum -a 256 ./* | LC_ALL=C /usr/bin/sort >SHA256SUMS.txt
)

ensure_output_directory "$CANDIDATE_PARENT" "$MACOS_ARTIFACT_ROOT"
if [[ -e "$FINAL_CANDIDATE_ROOT" || -L "$FINAL_CANDIDATE_ROOT" ]]; then
  echo "Candidate release output appeared during the build and will not be overwritten: $FINAL_CANDIDATE_ROOT" >&2
  exit 1
fi
"$PYTHON" - "$ARTIFACT_ROOT" "$FINAL_CANDIDATE_ROOT" <<'PY'
import os
import sys

os.rename(sys.argv[1], sys.argv[2])
PY
CANDIDATE_STAGE=""
ARTIFACT_ROOT="$FINAL_CANDIDATE_ROOT"
DMG="$ARTIFACT_ROOT/$(basename "$DMG")"
echo "Built local UNNOTARIZED release candidate: $DMG"
du -sh "$APP_BUNDLE" "$DMG"
