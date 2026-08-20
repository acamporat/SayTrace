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
"$FFMPEG" -hide_banner -bsfs >"$TEMPORARY_ROOT/bsfs.txt"
"$FFMPEG" -hide_banner -encoders >"$TEMPORARY_ROOT/encoders.txt"
"$FFMPEG" -hide_banner -muxers >"$TEMPORARY_ROOT/muxers.txt"
"$FFMPEG" -hide_banner -filters >"$TEMPORARY_ROOT/filters.txt"
"$FFMPEG" -hide_banner -protocols >"$TEMPORARY_ROOT/protocols.txt"

for component in aac aiff asf avi concat flac image2 loas m4v matroska mov mp3 mpeg mpegts mpegvideo ogg wav; do
  require_component "$TEMPORARY_ROOT/demuxers.txt" "$component"
done
for component in flac h264_videotoolbox mjpeg pcm_s16le wrapped_avframe; do
  require_component "$TEMPORARY_ROOT/encoders.txt" "$component"
done
for component in flac image2 mov mp4 null wav; do
  require_component "$TEMPORARY_ROOT/muxers.txt" "$component"
done
for component in adelay amix aresample asetpts asetrate concat format loudnorm scale setpts tpad trim; do
  require_component "$TEMPORARY_ROOT/filters.txt" "$component"
done
for component in file pipe; do
  require_component "$TEMPORARY_ROOT/protocols.txt" "$component"
done
require_component "$TEMPORARY_ROOT/bsfs.txt" h264_mp4toannexb

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

width = 160
height = 90
for frame_index in range(5):
    pixels = bytearray()
    for y in range(height):
        for x in range(width):
            pixels.extend(
                (
                    (x + frame_index * 24) % 256,
                    (y * 2 + frame_index * 12) % 256,
                    (x + y + frame_index * 8) % 256,
                )
            )
    (root / f"visual-frame-{frame_index:03d}.ppm").write_bytes(
        f"P6\n{width} {height}\n255\n".encode("ascii") + pixels
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

"$FFMPEG" -nostdin -hide_banner -loglevel error -y \
  -framerate 5 -start_number 0 -i "$TEMPORARY_ROOT/visual-frame-%03d.ppm" -an \
  -vf "scale=320:180,format=yuv420p,setpts=PTS-STARTPTS,tpad=stop_mode=clone:stop_duration=0.2" \
  -fps_mode cfr -r 5 -c:v h264_videotoolbox -q:v 65 -profile:v high \
  -g 10 -pix_fmt yuv420p -movflags +faststart -f mp4 \
  "$TEMPORARY_ROOT/visual-input.mp4"

"$FFMPEG" -nostdin -hide_banner -loglevel error -y \
  -i "$TEMPORARY_ROOT/visual-input.mp4" -map 0:v:0 -an -c:v copy \
  -movflags +faststart -f mp4 "$TEMPORARY_ROOT/visual-video-only.mp4"

"$FFMPEG" -nostdin -hide_banner -loglevel error -xerror \
  -i "$TEMPORARY_ROOT/visual-video-only.mp4" -map 0:v:0 -an -f null -

"$FFMPEG" -nostdin -hide_banner -loglevel error -y \
  -ss 0.2 -t 1.0 -i "$TEMPORARY_ROOT/visual-input.mp4" \
  -ss 0.4 -t 0.8 -i "$TEMPORARY_ROOT/visual-input.mp4" \
  -filter_complex \
  "[0:v]trim=start=0:duration=1.0,setpts=PTS-STARTPTS[v0];[1:v]trim=start=0:duration=0.8,setpts=PTS-STARTPTS[v1];[v0][v1]concat=n=2:v=1:a=0,tpad=start_mode=clone:start_duration=0.2,trim=duration=2.0,scale=320:180,format=yuv420p[vout]" \
  -map "[vout]" -an \
  -fps_mode cfr -r 5 -c:v h264_videotoolbox -q:v 65 -profile:v high \
  -g 10 -pix_fmt yuv420p -movflags +faststart -f mp4 \
  "$TEMPORARY_ROOT/visual-concatenated.mp4"

"$FFMPEG" -nostdin -hide_banner -loglevel error -xerror \
  -i "$TEMPORARY_ROOT/visual-concatenated.mp4" -map 0:v:0 -an -f null -

"$FFMPEG" -nostdin -hide_banner -loglevel error -y \
  -ss 0.2 -i "$TEMPORARY_ROOT/visual-concatenated.mp4" -map 0:v:0 \
  -frames:v 1 -vf "scale=min(1440\\,iw):-2" -c:v mjpeg -q:v 3 \
  -f image2 "$TEMPORARY_ROOT/extracted-frame.jpg"

"$FFPROBE" -v error -select_streams v:0 \
  -count_frames \
  -show_entries stream=codec_name,profile,pix_fmt,width,height,avg_frame_rate,nb_read_frames:format=duration \
  -of json "$TEMPORARY_ROOT/visual-concatenated.mp4" >"$TEMPORARY_ROOT/visual-probe.json"
"$FFPROBE" -v error -select_streams v:0 \
  -show_entries stream=codec_name,width,height \
  -of json "$TEMPORARY_ROOT/extracted-frame.jpg" >"$TEMPORARY_ROOT/frame-probe.json"
"$PYTHON" - "$TEMPORARY_ROOT/visual-probe.json" "$TEMPORARY_ROOT/frame-probe.json" <<'PY'
import json
import pathlib
import sys

visual = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
streams = visual.get("streams")
assert isinstance(streams, list) and len(streams) == 1, visual
stream = streams[0]
assert stream.get("codec_name") == "h264", stream
assert stream.get("profile") == "High", stream
assert stream.get("pix_fmt") == "yuv420p", stream
assert stream.get("width") == 320, stream
assert stream.get("height") == 180, stream
assert stream.get("avg_frame_rate") == "5/1", stream
assert stream.get("nb_read_frames") == "10", stream
duration = float(visual.get("format", {}).get("duration", "0"))
assert abs(duration - 2.0) <= 0.05, visual

frame = json.loads(pathlib.Path(sys.argv[2]).read_text(encoding="utf-8"))
streams = frame.get("streams")
assert isinstance(streams, list) and len(streams) == 1, frame
stream = streams[0]
assert stream.get("codec_name") == "mjpeg", stream
assert stream.get("width") == 320, stream
assert stream.get("height") == 180, stream
PY

echo "Verified release FFmpeg audio probing, normalization, concat, timing, resampling, delay, mixing, FLAC/WAV, VideoToolbox H.264 MP4, video-only stream-copy remux, strict video decode, per-input trim/setpts/filter-concat/tpad/scale/format, and JPEG/image2 frame extraction flows."
