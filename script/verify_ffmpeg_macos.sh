#!/bin/bash
set -euo pipefail

usage() {
  echo "Usage: script/verify_ffmpeg_macos.sh /path/to/ffmpeg-prefix" >&2
}

if (($# != 1)); then
  usage
  exit 2
fi

PREFIX="$(cd "$1" 2>/dev/null && pwd -P)" || {
  echo "FFmpeg prefix is unavailable: $1" >&2
  exit 1
}
FFMPEG="$PREFIX/bin/ffmpeg"
FFPROBE="$PREFIX/bin/ffprobe"
for executable in "$FFMPEG" "$FFPROBE"; do
  [[ -x "$executable" ]] || {
    echo "Missing media executable: $executable" >&2
    exit 1
  }
done

require_component() {
  local inventory="$1"
  local component="$2"
  if ! awk -v target="$component" \
    '{
      for (field = 1; field <= NF; field++) {
        count = split($field, names, ",")
        for (item = 1; item <= count; item++) {
          if (names[item] == target) {
            found = 1
          }
        }
      }
    }
    END { exit !found }' \
    "$inventory"; then
    echo "Release FFmpeg is missing required component: $component" >&2
    exit 1
  fi
}

TEMPORARY_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/saytrace-ffmpeg-verify.XXXXXX")"
cleanup_verification_root() {
  local verification_exit_code=$?
  set +e
  rm -rf -- "$TEMPORARY_ROOT"
  return "$verification_exit_code"
}
trap cleanup_verification_root EXIT

"$FFMPEG" -hide_banner -demuxers >"$TEMPORARY_ROOT/demuxers.txt"
"$FFMPEG" -hide_banner -encoders >"$TEMPORARY_ROOT/encoders.txt"
"$FFMPEG" -hide_banner -muxers >"$TEMPORARY_ROOT/muxers.txt"
"$FFMPEG" -hide_banner -filters >"$TEMPORARY_ROOT/filters.txt"
"$FFMPEG" -hide_banner -protocols >"$TEMPORARY_ROOT/protocols.txt"

for component in aac aiff asf avi concat flac loas m4v matroska mov mp3 mpeg mpegts mpegvideo ogg wav; do
  require_component "$TEMPORARY_ROOT/demuxers.txt" "$component"
done
for component in flac pcm_s16le; do
  require_component "$TEMPORARY_ROOT/encoders.txt" "$component"
done
for component in flac wav; do
  require_component "$TEMPORARY_ROOT/muxers.txt" "$component"
done
for component in adelay amix aresample asetpts asetrate loudnorm; do
  require_component "$TEMPORARY_ROOT/filters.txt" "$component"
done
require_component "$TEMPORARY_ROOT/protocols.txt" file

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
PYTHON="$SCRIPT_DIR/../worker/.venv/bin/python"
[[ -x "$PYTHON" ]] || PYTHON="$(command -v python3)"
"$PYTHON" - "$TEMPORARY_ROOT" <<'PY'
import math
import pathlib
import struct
import sys
import wave

root = pathlib.Path(sys.argv[1])
for name, frequency in (("first.wav", 440.0), ("second.wav", 660.0)):
    with wave.open(str(root / name), "wb") as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(48_000)
        frames = bytearray()
        for index in range(48_000):
            sample = round(8_000 * math.sin(2 * math.pi * frequency * index / 48_000))
            frames.extend(struct.pack("<h", sample))
        output.writeframes(frames)
(root / "segments.txt").write_text(
    "file 'first.wav'\nfile 'second.wav'\n", encoding="utf-8"
)
PY

"$FFMPEG" -nostdin -hide_banner -loglevel error -y \
  -f concat -safe 0 -i "$TEMPORARY_ROOT/segments.txt" -vn \
  -af "asetpts=PTS-STARTPTS,asetrate=48000,aresample=48000,adelay=10:all=1" \
  -c:a flac -compression_level 8 -f flac "$TEMPORARY_ROOT/concatenated.flac"

"$FFMPEG" -nostdin -hide_banner -loglevel error -y \
  -i "$TEMPORARY_ROOT/concatenated.flac" -i "$TEMPORARY_ROOT/second.wav" \
  -filter_complex "amix=inputs=2:duration=longest:dropout_transition=0" \
  -c:a flac -compression_level 8 -f flac "$TEMPORARY_ROOT/mixed.flac"

"$FFMPEG" -nostdin -hide_banner -loglevel error -y \
  -i "$TEMPORARY_ROOT/mixed.flac" -vn -ac 1 -ar 16000 \
  -af "loudnorm=I=-23:TP=-2:LRA=7" -c:a pcm_s16le \
  "$TEMPORARY_ROOT/normalized.wav"

"$FFPROBE" -v error -select_streams a:0 \
  -show_entries stream=codec_name,sample_rate,channels \
  -of json "$TEMPORARY_ROOT/normalized.wav" >"$TEMPORARY_ROOT/probe.json"
"$PYTHON" - "$TEMPORARY_ROOT/probe.json" <<'PY'
import json
import pathlib
import sys

data = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
streams = data.get("streams")
assert isinstance(streams, list) and len(streams) == 1, data
stream = streams[0]
assert stream.get("codec_name") == "pcm_s16le", stream
assert stream.get("sample_rate") == "16000", stream
assert stream.get("channels") == 1, stream
PY

echo "Verified release FFmpeg probing, normalization, concat, timing, resampling, delay, mixing, FLAC, and WAV flows."
