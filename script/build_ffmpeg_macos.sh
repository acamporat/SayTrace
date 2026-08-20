#!/bin/bash
set -euo pipefail

FFMPEG_VERSION="${SAYTRACE_FFMPEG_VERSION:-8.1.2}"
FFMPEG_RELEASE_KEY_FINGERPRINT="FCF986EA15E6E293A5644F10B4322F04D67658D8"

usage() {
  /bin/cat <<'EOF'
Build a redistributable LGPL-only FFmpeg/FFprobe pair for Apple Silicon.

Usage: script/build_ffmpeg_macos.sh [--rebuild | --verify-only]

Environment:
  SAYTRACE_FFMPEG_VERSION  FFmpeg release version (default: 8.1.2)
  SAYTRACE_GPG             GnuPG executable (defaults to PATH lookup)

Output:
  build/ffmpeg-macos-arm64/<version>/prefix/bin/{ffmpeg,ffprobe}

The source archive and detached signature are downloaded from ffmpeg.org and
verified against FFmpeg's published release-signing fingerprint before build.
EOF
}

REBUILD=0
VERIFY_ONLY=0
while (($#)); do
  case "$1" in
    --rebuild)
      REBUILD=1
      ;;
    --verify-only)
      VERIFY_ONLY=1
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
if ((REBUILD && VERIFY_ONLY)); then
  echo "--rebuild and --verify-only cannot be combined." >&2
  exit 2
fi

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "The release FFmpeg build must run natively on Apple Silicon macOS." >&2
  exit 1
fi
if [[ ! "$FFMPEG_VERSION" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; then
  echo "Invalid FFmpeg release version: $FFMPEG_VERSION" >&2
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
REPOSITORY_BUILD_ROOT="$REPOSITORY_ROOT/build"
FFMPEG_CACHE_PARENT="$REPOSITORY_BUILD_ROOT/ffmpeg-macos-arm64"
BUILD_ROOT="$REPOSITORY_ROOT/build/ffmpeg-macos-arm64/$FFMPEG_VERSION"
DOWNLOAD_ROOT="$BUILD_ROOT/downloads"
OUTPUT_PREFIX="$BUILD_ROOT/prefix"
ARCHIVE="$DOWNLOAD_ROOT/ffmpeg-$FFMPEG_VERSION.tar.xz"
SIGNATURE="$ARCHIVE.asc"
PUBLIC_KEY="$DOWNLOAD_ROOT/ffmpeg-devel.asc"
SOURCE_URL="https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz"
SIGNATURE_URL="$SOURCE_URL.asc"
PUBLIC_KEY_URL="https://ffmpeg.org/ffmpeg-devel.asc"
OUTPUT_FFMPEG="$OUTPUT_PREFIX/bin/ffmpeg"
OUTPUT_FFPROBE="$OUTPUT_PREFIX/bin/ffprobe"
GPG="${SAYTRACE_GPG:-$(command -v gpg || true)}"

for directory in "$REPOSITORY_BUILD_ROOT" "$FFMPEG_CACHE_PARENT" "$BUILD_ROOT"; do
  if [[ -L "$directory" || (-e "$directory" && ! -d "$directory") ]]; then
    echo "FFmpeg build path must be an ordinary directory: $directory" >&2
    exit 1
  fi
  if ((VERIFY_ONLY)); then
    [[ -d "$directory" ]] || {
      echo "Verified FFmpeg cache directory is missing: $directory" >&2
      exit 1
    }
  else
    /bin/mkdir -p "$directory"
  fi
  resolved_directory="$(cd "$directory" && pwd -P)"
  case "$resolved_directory" in
    "$REPOSITORY_BUILD_ROOT" | "$REPOSITORY_BUILD_ROOT"/*)
      ;;
    *)
      echo "FFmpeg build path resolves outside the repository build directory: $directory" >&2
      exit 1
      ;;
  esac
done
for directory in "$DOWNLOAD_ROOT" "$OUTPUT_PREFIX"; do
  if [[ -L "$directory" || (-e "$directory" && ! -d "$directory") ]]; then
    echo "FFmpeg cache path must be an ordinary directory: $directory" >&2
    exit 1
  fi
done

if [[ -z "$GPG" || ! -x "$GPG" ]]; then
  echo "Missing required command: gpg" >&2
  echo "Install GnuPG with: brew install gnupg" >&2
  exit 1
fi
if ((VERIFY_ONLY)); then
  for cached_file in "$ARCHIVE" "$SIGNATURE" "$PUBLIC_KEY"; do
    if [[ -L "$cached_file" || ! -s "$cached_file" ]]; then
      echo "Verified FFmpeg cache input is missing or unsafe: $cached_file" >&2
      exit 1
    fi
  done
  if [[ -L "$OUTPUT_PREFIX" || ! -x "$OUTPUT_FFMPEG" || ! -x "$OUTPUT_FFPROBE" ]]; then
    echo "No verified cached FFmpeg $FFMPEG_VERSION build is available." >&2
    exit 1
  fi
else
  for command_name in curl make tar xcrun; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
      echo "Missing required command: $command_name" >&2
      exit 1
    fi
  done
  /bin/mkdir -p "$DOWNLOAD_ROOT"
  for cached_file in "$ARCHIVE" "$SIGNATURE" "$PUBLIC_KEY"; do
    if [[ -L "$cached_file" ]]; then
      echo "Refusing symbolic-link FFmpeg cache input: $cached_file" >&2
      exit 1
    fi
  done
  if [[ ! -s "$ARCHIVE" ]]; then
    /usr/bin/curl --fail --location --proto '=https' --tlsv1.2 --output "$ARCHIVE" "$SOURCE_URL"
  fi
  if [[ ! -s "$SIGNATURE" ]]; then
    /usr/bin/curl --fail --location --proto '=https' --tlsv1.2 --output "$SIGNATURE" "$SIGNATURE_URL"
  fi
  if [[ ! -s "$PUBLIC_KEY" ]]; then
    /usr/bin/curl --fail --location --proto '=https' --tlsv1.2 --output "$PUBLIC_KEY" "$PUBLIC_KEY_URL"
  fi
fi

validate_binary() {
  local executable="$1"
  if [[ ! -x "$executable" ]]; then
    echo "Missing FFmpeg release executable: $executable" >&2
    return 1
  fi
  if ! file -L "$executable" | grep -q 'Mach-O 64-bit executable arm64'; then
    echo "FFmpeg release executable is not arm64 Mach-O: $executable" >&2
    return 1
  fi
  local external_libraries
  external_libraries="$(otool -L "$executable" | awk 'NR > 1 && $1 ~ /^\// { print $1 }' | grep -Ev '^(/usr/lib/|/System/Library/)' || true)"
  if [[ -n "$external_libraries" ]]; then
    echo "FFmpeg release executable has non-system dynamic dependencies:" >&2
    echo "$external_libraries" >&2
    return 1
  fi
}

has_buildconf_option() {
  local configuration="$1"
  local required="$2"
  local line
  local prefix=""
  local value=""
  local quoted=""
  if [[ "$required" == *=* ]]; then
    prefix="${required%%=*}="
    value="${required#*=}"
    quoted="${prefix}'${value}'"
  fi
  while IFS= read -r line; do
    line="${line#"${line%%[![:space:]]*}"}"
    if [[ "$line" == "$required" || (-n "$quoted" && "$line" == "$quoted") ]]; then
      return 0
    fi
  done <<<"$configuration"
  return 1
}

validate_build_identity() {
  local ffmpeg="$1"
  local ffprobe="$2"
  local version_line
  local probe_version_line
  local configuration
  version_line="$("$ffmpeg" -version | head -1)"
  probe_version_line="$("$ffprobe" -version | head -1)"
  if [[ "$version_line" != "ffmpeg version $FFMPEG_VERSION "* ]]; then
    echo "FFmpeg binary version does not match verified source $FFMPEG_VERSION." >&2
    return 1
  fi
  if [[ "$probe_version_line" != "ffprobe version $FFMPEG_VERSION "* ]]; then
    echo "FFprobe binary version does not match verified source $FFMPEG_VERSION." >&2
    return 1
  fi
  configuration="$("$ffmpeg" -buildconf 2>&1)"
  if has_buildconf_option "$configuration" "--enable-gpl" || \
    has_buildconf_option "$configuration" "--enable-nonfree"; then
    echo "FFmpeg build is not LGPL-compatible." >&2
    return 1
  fi
  for required_option in \
    --disable-autodetect \
    --enable-zlib \
    --enable-bzlib \
    --enable-videotoolbox \
    --disable-gpl \
    --disable-nonfree \
    --disable-version3 \
    --disable-network \
    --disable-shared \
    --enable-static \
    --enable-swscale \
    --disable-hwaccels \
    --disable-encoders \
    --enable-encoder=flac,h264_videotoolbox,mjpeg,pcm_s16le,wrapped_avframe \
    --disable-muxers \
    --enable-muxer=flac,image2,mov,mp4,null,wav \
    --disable-demuxers \
    --enable-demuxer=aac,aiff,asf,avi,concat,flac,image2,loas,m4v,matroska,mov,mp3,mpegps,mpegts,mpegvideo,ogg,wav \
    --disable-protocols \
    --enable-protocol=file,pipe \
    --disable-filters \
    --enable-filter=adelay,amix,aresample,asetpts,asetrate,concat,format,loudnorm,scale,setpts,tpad,trim \
    --disable-bsfs \
    --enable-bsf=h264_mp4toannexb; do
    if ! has_buildconf_option "$configuration" "$required_option"; then
      echo "FFmpeg build is missing required configuration: $required_option" >&2
      return 1
    fi
  done
}

# GnuPG's agent socket has a short platform path limit. Keep the disposable
# verification keyring under the system temp directory rather than the
# potentially long repository path.
GNUPG_HOME="$(mktemp -d "${TMPDIR:-/tmp}/saytrace-gnupg.XXXXXX")"
WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/saytrace-ffmpeg-build.XXXXXX")"
SOURCE_PARENT="$WORK_ROOT/source"
SOURCE_ROOT="$SOURCE_PARENT/ffmpeg-$FFMPEG_VERSION"
PREFIX="$WORK_ROOT/prefix"
FFMPEG="$PREFIX/bin/ffmpeg"
FFPROBE="$PREFIX/bin/ffprobe"
cleanup_ffmpeg_stages() {
  local ffmpeg_exit_code=$?
  set +e
  rm -rf -- "$GNUPG_HOME" "$WORK_ROOT"
  return "$ffmpeg_exit_code"
}
trap cleanup_ffmpeg_stages EXIT
/bin/chmod 0700 "$GNUPG_HOME"
GNUPGHOME="$GNUPG_HOME" "$GPG" --batch --quiet --import "$PUBLIC_KEY"
IMPORTED_FINGERPRINT="$(GNUPGHOME="$GNUPG_HOME" "$GPG" --batch --with-colons --fingerprint ffmpeg-devel@ffmpeg.org | awk -F: '$1 == "fpr" { print $10; exit }')"
if [[ "$IMPORTED_FINGERPRINT" != "$FFMPEG_RELEASE_KEY_FINGERPRINT" ]]; then
  echo "FFmpeg release key fingerprint mismatch." >&2
  exit 1
fi
VERIFY_STATUS="$(GNUPGHOME="$GNUPG_HOME" "$GPG" --batch --status-fd 1 --verify "$SIGNATURE" "$ARCHIVE" 2>/dev/null)"
if ! grep -q "^\[GNUPG:\] VALIDSIG $FFMPEG_RELEASE_KEY_FINGERPRINT " <<<"$VERIFY_STATUS"; then
  echo "FFmpeg source signature validation failed." >&2
  exit 1
fi

if ((REBUILD == 0)) && [[ -x "$OUTPUT_FFMPEG" && -x "$OUTPUT_FFPROBE" ]]; then
  PROVENANCE_ROOT="$OUTPUT_PREFIX/share/saytrace-ffmpeg"
  BINARY_CHECKSUMS="$PROVENANCE_ROOT/binary-sha256.txt"
  validate_binary "$OUTPUT_FFMPEG"
  validate_binary "$OUTPUT_FFPROBE"
  validate_build_identity "$OUTPUT_FFMPEG" "$OUTPUT_FFPROBE"
  "$SCRIPT_DIR/verify_ffmpeg_macos.sh" "$OUTPUT_PREFIX"
  [[ "$(/bin/cat "$PROVENANCE_ROOT/source-url.txt")" == "$SOURCE_URL" ]] || {
    echo "Cached FFmpeg source URL does not match the requested release." >&2
    exit 1
  }
  [[ "$(/bin/cat "$PROVENANCE_ROOT/source-signing-key-fingerprint.txt")" == "$FFMPEG_RELEASE_KEY_FINGERPRINT" ]] || {
    echo "Cached FFmpeg source-signing fingerprint is invalid." >&2
    exit 1
  }
  ARCHIVE_SHA256="$(/usr/bin/shasum -a 256 "$ARCHIVE" | awk '{print $1}')"
  [[ "$(/bin/cat "$PROVENANCE_ROOT/source-sha256.txt")" == "$ARCHIVE_SHA256" ]] || {
    echo "Cached FFmpeg source hash does not match the verified archive." >&2
    exit 1
  }
  [[ -f "$BINARY_CHECKSUMS" ]] || {
    echo "Cached FFmpeg binary checksum inventory is missing; use --rebuild." >&2
    exit 1
  }
  (cd "$OUTPUT_PREFIX" && /usr/bin/shasum -a 256 -c "share/saytrace-ffmpeg/binary-sha256.txt")
  echo "Using verified cached FFmpeg build: $OUTPUT_PREFIX"
  exit 0
fi
if ((VERIFY_ONLY)); then
  echo "No verified cached FFmpeg $FFMPEG_VERSION build is available." >&2
  exit 1
fi

# FFmpeg writes its configured prefix to a shell fragment without quoting it.
# Build under the short system temporary path so repositories containing spaces
# cannot turn path components into shell commands during the build.
/bin/mkdir -p "$SOURCE_PARENT" "$PREFIX"
/usr/bin/tar -xf "$ARCHIVE" -C "$SOURCE_PARENT"

if [[ -d /Applications/Xcode.app/Contents/Developer ]]; then
  export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
fi
CLANG="$(xcrun --find clang)"
AR="$(xcrun --find ar)"
RANLIB="$(xcrun --find ranlib)"
SDK_ROOT="$(xcrun --sdk macosx --show-sdk-path)"
(
  cd "$SOURCE_ROOT"
  MACOSX_DEPLOYMENT_TARGET=15.0 SDKROOT="$SDK_ROOT" ./configure \
    --prefix="$PREFIX" \
    --arch=arm64 \
    --target-os=darwin \
    --cc="$CLANG" \
    --ar="$AR" \
    --ranlib="$RANLIB -D" \
    --host-cc="$CLANG" \
    --host-cflags="-O3 -mmacosx-version-min=15.0 -isysroot $SDK_ROOT" \
    --host-cppflags="-isysroot $SDK_ROOT" \
    --host-ld="$CLANG" \
    --host-ldflags="-mmacosx-version-min=15.0 -isysroot $SDK_ROOT" \
    --sysroot="$SDK_ROOT" \
    --disable-autodetect \
    --enable-zlib \
    --enable-bzlib \
    --enable-videotoolbox \
    --disable-debug \
    --disable-doc \
    --disable-ffplay \
    --disable-avdevice \
    --disable-network \
    --enable-swscale \
    --disable-hwaccels \
    --disable-encoders \
    --enable-encoder=flac,h264_videotoolbox,mjpeg,pcm_s16le,wrapped_avframe \
    --disable-muxers \
    --enable-muxer=flac,image2,mov,mp4,null,wav \
    --disable-demuxers \
    --enable-demuxer=aac,aiff,asf,avi,concat,flac,image2,loas,m4v,matroska,mov,mp3,mpegps,mpegts,mpegvideo,ogg,wav \
    --disable-protocols \
    --enable-protocol=file,pipe \
    --disable-filters \
    --enable-filter=adelay,amix,aresample,asetpts,asetrate,concat,format,loudnorm,scale,setpts,tpad,trim \
    --disable-bsfs \
    --enable-bsf=h264_mp4toannexb \
    --disable-shared \
    --enable-static \
    --enable-pic \
    --disable-gpl \
    --disable-nonfree \
    --disable-version3 \
    --extra-cflags="-O3 -mmacosx-version-min=15.0 -isysroot $SDK_ROOT" \
    --extra-ldflags="-mmacosx-version-min=15.0 -isysroot $SDK_ROOT"
)

make -C "$SOURCE_ROOT" -j"$(sysctl -n hw.logicalcpu)"
make -C "$SOURCE_ROOT" install
/usr/bin/strip -x "$FFMPEG" "$FFPROBE"

validate_binary "$FFMPEG"
validate_binary "$FFPROBE"
validate_build_identity "$FFMPEG" "$FFPROBE"
"$SCRIPT_DIR/verify_ffmpeg_macos.sh" "$PREFIX"
BUILD_CONFIGURATION="$("$FFMPEG" -buildconf 2>&1)"

PROVENANCE_ROOT="$PREFIX/share/saytrace-ffmpeg"
/bin/mkdir -p "$PROVENANCE_ROOT"
/bin/cp "$SOURCE_ROOT/COPYING.LGPLv2.1" "$PROVENANCE_ROOT/"
/bin/cp "$SOURCE_ROOT/LICENSE.md" "$PROVENANCE_ROOT/"
/usr/bin/printf '%s\n' "$SOURCE_URL" >"$PROVENANCE_ROOT/source-url.txt"
/usr/bin/printf '%s\n' "$FFMPEG_RELEASE_KEY_FINGERPRINT" >"$PROVENANCE_ROOT/source-signing-key-fingerprint.txt"
/usr/bin/shasum -a 256 "$ARCHIVE" | awk '{print $1}' >"$PROVENANCE_ROOT/source-sha256.txt"
/usr/bin/printf '%s\n' "$BUILD_CONFIGURATION" >"$PROVENANCE_ROOT/build-configuration.txt"
(
  cd "$PREFIX"
  /usr/bin/shasum -a 256 bin/ffmpeg bin/ffprobe >"share/saytrace-ffmpeg/binary-sha256.txt"
)

# Publish the verified build to the fixed repository-local cache only after all
# validation and provenance generation have succeeded.
for directory in "$REPOSITORY_BUILD_ROOT" "$FFMPEG_CACHE_PARENT" "$BUILD_ROOT"; do
  if [[ -L "$directory" || ! -d "$directory" ]]; then
    echo "FFmpeg build path became unsafe before cache promotion: $directory" >&2
    exit 1
  fi
  resolved_directory="$(cd "$directory" && pwd -P)"
  case "$resolved_directory" in
    "$REPOSITORY_BUILD_ROOT" | "$REPOSITORY_BUILD_ROOT"/*)
      ;;
    *)
      echo "FFmpeg build path resolves outside the repository build directory: $directory" >&2
      exit 1
      ;;
  esac
done
if [[ -L "$OUTPUT_PREFIX" || (-e "$OUTPUT_PREFIX" && ! -d "$OUTPUT_PREFIX") ]]; then
  echo "FFmpeg cache output became unsafe before promotion: $OUTPUT_PREFIX" >&2
  exit 1
fi
rm -rf -- "$OUTPUT_PREFIX"
/bin/mkdir -p "$(dirname "$OUTPUT_PREFIX")"
/usr/bin/ditto "$PREFIX" "$OUTPUT_PREFIX"
validate_binary "$OUTPUT_FFMPEG"
validate_binary "$OUTPUT_FFPROBE"

echo "Built verified LGPL FFmpeg $FFMPEG_VERSION: $OUTPUT_PREFIX"
du -sh "$OUTPUT_PREFIX"
