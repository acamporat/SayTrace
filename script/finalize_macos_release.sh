#!/bin/bash
set -euo pipefail

usage() {
  /bin/cat <<'EOF'
Promote one exact, physically accepted prepared macOS installer.

Usage: script/finalize_macos_release.sh

Environment:
  SAYTRACE_MACOS_ACCEPTANCE_EVIDENCE  completed schema-2 acceptance JSON
  SAYTRACE_NOTARY_PROFILE             notarytool Keychain profile
  SAYTRACE_FFMPEG_VERSION             prepared FFmpeg version (default: 8.1.2)
EOF
}

if (($#)); then
  case "$1" in
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
fi
if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "macOS release finalization must run on Apple Silicon." >&2
  exit 1
fi
PATH="/usr/bin:/bin:/usr/sbin:/sbin"
export PATH
if [[ -z "${SAYTRACE_MACOS_ACCEPTANCE_EVIDENCE:-}" ]]; then
  echo "SAYTRACE_MACOS_ACCEPTANCE_EVIDENCE is required." >&2
  exit 1
fi
if [[ -z "${SAYTRACE_NOTARY_PROFILE:-}" ]]; then
  echo "SAYTRACE_NOTARY_PROFILE is required for fresh Apple submission checks." >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
EXPECTED_TEAM_ID="ZQF63BSBBN"

ensure_output_directory() {
  local directory="$1"
  local allowed_root="$2"
  local resolved_allowed_root
  local resolved_directory
  if [[ -L "$allowed_root" || ! -d "$allowed_root" ]]; then
    echo "Release output root must be an ordinary directory: $allowed_root" >&2
    return 1
  fi
  if [[ -L "$directory" || (-e "$directory" && ! -d "$directory") ]]; then
    echo "Release output path must be an ordinary directory: $directory" >&2
    return 1
  fi
  /bin/mkdir -p "$directory"
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

assert_existing_output_directory() {
  local directory="$1"
  local allowed_root="$2"
  if [[ -L "$directory" || ! -d "$directory" ]]; then
    echo "Required release directory is missing or unsafe: $directory" >&2
    return 1
  fi
  ensure_output_directory "$directory" "$allowed_root"
}

PYTHON="$REPOSITORY_ROOT/worker/.venv/bin/python"
[[ -x "$PYTHON" ]] || {
  echo "The locked worker environment is missing." >&2
  exit 1
}
VERSION="$("$PYTHON" -c 'import json,pathlib,sys; print(json.loads(pathlib.Path(sys.argv[1]).read_text())["version"])' "$REPOSITORY_ROOT/package.json")"
[[ "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || {
  echo "Release version must be stable semantic versioning." >&2
  exit 1
}
SOURCE_REVISION="$(git -C "$REPOSITORY_ROOT" rev-parse HEAD)"
SOURCE_STATUS="$(git -C "$REPOSITORY_ROOT" status --porcelain=v1 --untracked-files=all)"
[[ -z "$SOURCE_STATUS" ]] || {
  echo "Release finalization requires a completely clean worktree:" >&2
  echo "$SOURCE_STATUS" >&2
  exit 1
}
if git -C "$REPOSITORY_ROOT" rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
  echo "Release tag v$VERSION already exists." >&2
  exit 1
fi

FFMPEG_VERSION="${SAYTRACE_FFMPEG_VERSION:-8.1.2}"
REPOSITORY_ARTIFACT_ROOT="$REPOSITORY_ROOT/artifacts"
ARTIFACT_PARENT="$REPOSITORY_ARTIFACT_ROOT/macos"
PREPARED_PARENT="$ARTIFACT_PARENT/prepared"
PREPARED_ROOT="$PREPARED_PARENT/v$VERSION"
FINAL_ROOT="$ARTIFACT_PARENT/v$VERSION"
PREPARED_DMG="$PREPARED_ROOT/SayTrace-$VERSION-macos-arm64.dmg"
PREPARED_SOURCE="$PREPARED_ROOT/SayTrace-$VERSION-ffmpeg-$FFMPEG_VERSION-source.tar.xz"
PREPARED_SOURCE_SIGNATURE="$PREPARED_SOURCE.asc"
STATE_ROOT="$PREPARED_ROOT/.release-state"
APP_ZIP="$STATE_ROOT/SayTrace-$VERSION-macos-arm64.zip"
APP_NOTARY_RESULT="$STATE_ROOT/app-notarization.json"
DMG_NOTARY_RESULT="$STATE_ROOT/dmg-notarization.json"

ensure_output_directory "$REPOSITORY_ARTIFACT_ROOT" "$REPOSITORY_ROOT"
ensure_output_directory "$ARTIFACT_PARENT" "$REPOSITORY_ARTIFACT_ROOT"
assert_existing_output_directory "$PREPARED_PARENT" "$ARTIFACT_PARENT"
assert_existing_output_directory "$PREPARED_ROOT" "$PREPARED_PARENT"
[[ ! -e "$FINAL_ROOT" && ! -L "$FINAL_ROOT" ]] || {
  echo "Final release output already exists and will not be overwritten: $FINAL_ROOT" >&2
  exit 1
}
"$PYTHON" "$REPOSITORY_ROOT/script/verify_macos_prepared_release.py" \
  --prepared-root "$PREPARED_ROOT" \
  --version "$VERSION" \
  --source-revision "$SOURCE_REVISION" \
  --ffmpeg-version "$FFMPEG_VERSION"
SAYTRACE_GPG=/opt/homebrew/bin/gpg \
  "$REPOSITORY_ROOT/script/build_ffmpeg_macos.sh" --verify-only
/usr/bin/cmp "$PREPARED_SOURCE" "$REPOSITORY_ROOT/build/ffmpeg-macos-arm64/$FFMPEG_VERSION/downloads/ffmpeg-$FFMPEG_VERSION.tar.xz"
/usr/bin/cmp "$PREPARED_SOURCE_SIGNATURE" "$REPOSITORY_ROOT/build/ffmpeg-macos-arm64/$FFMPEG_VERSION/downloads/ffmpeg-$FFMPEG_VERSION.tar.xz.asc"
/usr/bin/cmp "$PREPARED_ROOT/RELEASE_NOTES.md" "$REPOSITORY_ROOT/docs/releases/v$VERSION.md"

APP_NOTARIZATION_ID="$("$PYTHON" -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$APP_NOTARY_RESULT")"
DMG_NOTARIZATION_ID="$("$PYTHON" -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$DMG_NOTARY_RESULT")"
TEMPORARY_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/saytrace-release-finalize.XXXXXX")"
ensure_output_directory "$ARTIFACT_PARENT" "$REPOSITORY_ARTIFACT_ROOT"
FINAL_STAGE="$(mktemp -d "$ARTIFACT_PARENT/.v$VERSION-final.XXXXXX")"
MOUNT_POINT="$TEMPORARY_ROOT/mount"
/bin/mkdir "$MOUNT_POINT"
MOUNTED=0
cleanup() {
  local finalizer_exit_code=$?
  set +e
  if ((MOUNTED)); then
    /usr/bin/hdiutil detach "$MOUNT_POINT" >/dev/null 2>&1 || true
  fi
  if [[ -n "${FINAL_STAGE:-}" && "$FINAL_STAGE" == "$ARTIFACT_PARENT/.v$VERSION-final."* && -d "$FINAL_STAGE" ]]; then
    rm -rf -- "$FINAL_STAGE"
  fi
  rm -rf -- "$TEMPORARY_ROOT"
  return "$finalizer_exit_code"
}
trap cleanup EXIT

xcrun notarytool info "$APP_NOTARIZATION_ID" \
  --keychain-profile "$SAYTRACE_NOTARY_PROFILE" \
  --output-format json >"$TEMPORARY_ROOT/app-notary-info.json"
xcrun notarytool info "$DMG_NOTARIZATION_ID" \
  --keychain-profile "$SAYTRACE_NOTARY_PROFILE" \
  --output-format json >"$TEMPORARY_ROOT/dmg-notary-info.json"
"$PYTHON" - \
  "$TEMPORARY_ROOT/app-notary-info.json" "$APP_NOTARIZATION_ID" "$(basename "$APP_ZIP")" \
  "$TEMPORARY_ROOT/dmg-notary-info.json" "$DMG_NOTARIZATION_ID" "$(basename "$PREPARED_DMG")" <<'PY'
import json
import pathlib
import sys

for offset in range(1, len(sys.argv), 3):
    path = pathlib.Path(sys.argv[offset])
    expected_id = sys.argv[offset + 1].lower()
    expected_name = sys.argv[offset + 2]
    result = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(result, dict):
        raise SystemExit(f"notary info is malformed: {path}")
    if result.get("status") != "Accepted":
        raise SystemExit(f"notary submission is not Accepted: {path}")
    if str(result.get("id", "")).lower() != expected_id:
        raise SystemExit(f"notary submission ID changed: {path}")
    if result.get("name") != expected_name:
        raise SystemExit(f"notary submission name changed: {path}")
PY

DMG="$FINAL_STAGE/$(basename "$PREPARED_DMG")"
SOURCE_ASSET="$FINAL_STAGE/$(basename "$PREPARED_SOURCE")"
SOURCE_SIGNATURE_ASSET="$FINAL_STAGE/$(basename "$PREPARED_SOURCE_SIGNATURE")"
RELEASE_NOTES="$FINAL_STAGE/RELEASE_NOTES.md"
ACCEPTANCE_ASSET="$FINAL_STAGE/SayTrace-$VERSION-macos-physical-acceptance.json"
/bin/cp "$PREPARED_DMG" "$DMG"
/bin/cp "$PREPARED_SOURCE" "$SOURCE_ASSET"
/bin/cp "$PREPARED_SOURCE_SIGNATURE" "$SOURCE_SIGNATURE_ASSET"
/bin/cp "$PREPARED_ROOT/RELEASE_NOTES.md" "$RELEASE_NOTES"
/bin/cp "$SAYTRACE_MACOS_ACCEPTANCE_EVIDENCE" "$ACCEPTANCE_ASSET"

"$PYTHON" "$REPOSITORY_ROOT/script/verify_macos_release_evidence.py" \
  --evidence "$ACCEPTANCE_ASSET" \
  --version "$VERSION" \
  --source-revision "$SOURCE_REVISION" \
  --installer "$DMG"
/usr/bin/hdiutil verify "$DMG"
/usr/bin/hdiutil attach "$DMG" \
  -readonly -nobrowse -noautoopen -mountpoint "$MOUNT_POINT" >/dev/null
MOUNTED=1
MOUNTED_APP="$MOUNT_POINT/SayTrace.app"
[[ ! -L "$MOUNTED_APP" && -d "$MOUNTED_APP" ]] || {
  echo "Prepared DMG does not contain an ordinary SayTrace.app." >&2
  exit 1
}
MOUNT_INVENTORY="$(/usr/bin/find "$MOUNT_POINT" -mindepth 1 -maxdepth 1 -print | /usr/bin/sed "s#^$MOUNT_POINT/##" | LC_ALL=C /usr/bin/sort)"
[[ "$MOUNT_INVENTORY" == $'Applications\nSayTrace.app' ]] || {
  echo "Prepared DMG root inventory is not exact:" >&2
  echo "$MOUNT_INVENTORY" >&2
  exit 1
}
[[ -L "$MOUNT_POINT/Applications" && "$(/usr/bin/readlink "$MOUNT_POINT/Applications")" == "/Applications" ]] || {
  echo "Prepared DMG Applications link is invalid." >&2
  exit 1
}

/usr/bin/codesign --verify --deep --strict --verbose=2 "$MOUNTED_APP"
/usr/bin/codesign --verify --strict --verbose=2 "$DMG"
xcrun stapler validate "$MOUNTED_APP"
xcrun stapler validate "$DMG"
/usr/sbin/spctl --assess --type execute --verbose=4 "$MOUNTED_APP"
/usr/sbin/spctl --assess --type open --context context:primary-signature --verbose=4 "$DMG"
APP_SIGNATURE="$(/usr/bin/codesign --display --verbose=4 "$MOUNTED_APP" 2>&1)"
DMG_SIGNATURE="$(/usr/bin/codesign --display --verbose=4 "$DMG" 2>&1)"
grep -Fq 'Authority=Developer ID Application:' <<<"$APP_SIGNATURE" || {
  echo "Mounted app is not signed with Developer ID Application." >&2
  exit 1
}
grep -Fq 'Authority=Developer ID Application:' <<<"$DMG_SIGNATURE" || {
  echo "DMG is not signed with Developer ID Application." >&2
  exit 1
}
APP_TEAM="$(sed -n 's/^TeamIdentifier=//p' <<<"$APP_SIGNATURE" | head -1)"
DMG_TEAM="$(sed -n 's/^TeamIdentifier=//p' <<<"$DMG_SIGNATURE" | head -1)"
[[ "$APP_TEAM" == "$EXPECTED_TEAM_ID" ]] || {
  echo "Mounted app TeamIdentifier must be $EXPECTED_TEAM_ID." >&2
  exit 1
}
[[ "$DMG_TEAM" == "$EXPECTED_TEAM_ID" ]] || {
  echo "DMG TeamIdentifier must be $EXPECTED_TEAM_ID." >&2
  exit 1
}
INFO_PLIST="$MOUNTED_APP/Contents/Info.plist"
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$INFO_PLIST")" == "com.localtranscript.desktop" ]] || {
  echo "Mounted app bundle identifier is invalid." >&2
  exit 1
}
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$INFO_PLIST")" == "$VERSION" ]] || {
  echo "Mounted app version is invalid." >&2
  exit 1
}
APP_BINARY="$MOUNTED_APP/Contents/MacOS/local-transcript"
file -L "$APP_BINARY" | grep -q 'Mach-O 64-bit executable arm64' || {
  echo "Mounted app executable is not arm64." >&2
  exit 1
}
"$PYTHON" "$REPOSITORY_ROOT/script/verify_macos_macho.py" --path "$APP_BINARY"
MOUNTED_RUNTIME="$MOUNTED_APP/Contents/Resources/runtime"
MOUNTED_RUNTIME_MANIFEST="$MOUNTED_RUNTIME/runtime-manifest.json"
"$REPOSITORY_ROOT/script/verify_macos_runtime.py" \
  --runtime "$MOUNTED_RUNTIME" \
  --version "$VERSION" \
  --source-revision "$SOURCE_REVISION"
"$PYTHON" "$REPOSITORY_ROOT/script/verify_macos_prepared_release.py" \
  --prepared-root "$PREPARED_ROOT" \
  --version "$VERSION" \
  --source-revision "$SOURCE_REVISION" \
  --ffmpeg-version "$FFMPEG_VERSION" \
  --runtime-manifest "$MOUNTED_RUNTIME_MANIFEST"

MANIFEST="$FINAL_STAGE/SayTrace-$VERSION-macos-arm64.release-manifest.json"
"$PYTHON" "$REPOSITORY_ROOT/script/generate_macos_release_manifest.py" \
  --version "$VERSION" \
  --source-revision "$SOURCE_REVISION" \
  --dmg "$DMG" \
  --runtime-manifest "$MOUNTED_RUNTIME_MANIFEST" \
  --ffmpeg-source "$SOURCE_ASSET" \
  --ffmpeg-source-signature "$SOURCE_SIGNATURE_ASSET" \
  --official \
  --app "$MOUNTED_APP" \
  --app-notary-result "$APP_NOTARY_RESULT" \
  --app-notary-payload "$APP_ZIP" \
  --dmg-notary-result "$DMG_NOTARY_RESULT" \
  --acceptance-evidence "$ACCEPTANCE_ASSET" \
  --output "$MANIFEST"

/usr/bin/hdiutil detach "$MOUNT_POINT" >/dev/null
MOUNTED=0
(
  cd "$FINAL_STAGE"
  /usr/bin/shasum -a 256 \
    "$(basename "$DMG")" \
    "$(basename "$MANIFEST")" \
    "$(basename "$SOURCE_ASSET")" \
    "$(basename "$SOURCE_SIGNATURE_ASSET")" \
    "$(basename "$ACCEPTANCE_ASSET")" \
    "$(basename "$RELEASE_NOTES")" \
    | LC_ALL=C /usr/bin/sort >SHA256SUMS.txt
)
ensure_output_directory "$ARTIFACT_PARENT" "$REPOSITORY_ARTIFACT_ROOT"
if [[ -e "$FINAL_ROOT" || -L "$FINAL_ROOT" ]]; then
  echo "Final release output appeared during verification and will not be overwritten: $FINAL_ROOT" >&2
  exit 1
fi
"$PYTHON" - "$FINAL_STAGE" "$FINAL_ROOT" <<'PY'
import os
import sys

os.rename(sys.argv[1], sys.argv[2])
PY
FINAL_STAGE=""
echo "Finalized exact physically accepted macOS release: $FINAL_ROOT"
