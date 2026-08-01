"""Versioned binary framing for the inherited stdin/stdout pipes.

Frame layout (network byte order):

    magic[4] = b"LTW1"
    version_major: u8
    kind: u8
    flags: u16
    payload_length: u64
    payload[payload_length]

JSON frames contain UTF-8 JSON. Audio frames contain a u32 metadata JSON length,
the metadata JSON, then raw PCM bytes. The worker emits JSON frames only.
"""

from __future__ import annotations

import io
import json
import struct
import threading
from dataclasses import dataclass
from enum import IntEnum
from typing import Any, BinaryIO

from .errors import ErrorCode, WorkerError
from .schema import AudioMetadata, JsonObject

MAGIC = b"LTW1"
HEADER = struct.Struct(">4sBBHQ")
AUDIO_META_LENGTH = struct.Struct(">I")
MAX_CONTROL_BYTES = 4 * 1024 * 1024
MAX_AUDIO_BYTES = 64 * 1024 * 1024


class FrameKind(IntEnum):
    JSON = 1
    AUDIO = 2


@dataclass(frozen=True, slots=True)
class Frame:
    kind: FrameKind
    payload: bytes
    flags: int = 0


@dataclass(frozen=True, slots=True)
class AudioFrame:
    metadata: AudioMetadata
    pcm: bytes


def _read_exact(stream: BinaryIO, length: int) -> bytes:
    chunks: list[bytes] = []
    remaining = length
    while remaining:
        chunk = stream.read(remaining)
        if not chunk:
            if remaining == length:
                raise EOFError
            raise WorkerError(ErrorCode.BAD_FRAME, "Unexpected EOF inside a protocol frame.")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def encode_frame(frame: Frame) -> bytes:
    limit = MAX_AUDIO_BYTES if frame.kind is FrameKind.AUDIO else MAX_CONTROL_BYTES
    if len(frame.payload) > limit:
        raise WorkerError(ErrorCode.BAD_FRAME, "Frame exceeds the configured size limit.")
    return HEADER.pack(MAGIC, 1, int(frame.kind), frame.flags, len(frame.payload)) + frame.payload


def encode_json(data: JsonObject) -> bytes:
    try:
        payload = json.dumps(
            data, ensure_ascii=False, allow_nan=False, separators=(",", ":")
        ).encode("utf-8")
    except (TypeError, ValueError) as exc:
        raise WorkerError(ErrorCode.BAD_FRAME, "Output event is not valid JSON.") from exc
    return encode_frame(Frame(FrameKind.JSON, payload))


def encode_audio(metadata: JsonObject, pcm: bytes) -> bytes:
    meta = json.dumps(metadata, allow_nan=False, separators=(",", ":")).encode("utf-8")
    payload = AUDIO_META_LENGTH.pack(len(meta)) + meta + pcm
    return encode_frame(Frame(FrameKind.AUDIO, payload))


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"Non-finite JSON number {value!r} is not allowed.")


def _unique_json_object(pairs: list[tuple[str, Any]]) -> JsonObject:
    result: JsonObject = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"Duplicate JSON key {key!r}.")
        result[key] = value
    return result


def _load_json(payload: bytes, description: str) -> Any:
    try:
        return json.loads(
            payload.decode("utf-8"),
            parse_constant=_reject_json_constant,
            object_pairs_hook=_unique_json_object,
        )
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError, ValueError) as exc:
        raise WorkerError(ErrorCode.BAD_FRAME, f"Malformed {description} JSON.") from exc


class FrameReader:
    def __init__(self, stream: BinaryIO) -> None:
        self._stream = stream

    def read(self) -> Frame:
        raw_header = _read_exact(self._stream, HEADER.size)
        magic, major, raw_kind, flags, length = HEADER.unpack(raw_header)
        if magic != MAGIC or major != 1:
            raise WorkerError(ErrorCode.BAD_FRAME, "Invalid frame magic or major version.")
        try:
            kind = FrameKind(raw_kind)
        except ValueError as exc:
            raise WorkerError(ErrorCode.BAD_FRAME, f"Unknown frame kind {raw_kind}.") from exc
        limit = MAX_AUDIO_BYTES if kind is FrameKind.AUDIO else MAX_CONTROL_BYTES
        if length > limit:
            raise WorkerError(
                ErrorCode.BAD_FRAME,
                f"{kind.name.lower()} payload exceeds {limit} bytes.",
            )
        return Frame(kind, _read_exact(self._stream, length), flags)

    @staticmethod
    def parse_json(frame: Frame) -> JsonObject:
        if frame.kind is not FrameKind.JSON:
            raise WorkerError(ErrorCode.BAD_FRAME, "Expected a JSON control frame.")
        value = _load_json(frame.payload, "control")
        if not isinstance(value, dict) or any(not isinstance(key, str) for key in value):
            raise WorkerError(ErrorCode.BAD_FRAME, "Control payload must be a JSON object.")
        return value

    @staticmethod
    def parse_audio(frame: Frame) -> AudioFrame:
        if frame.kind is not FrameKind.AUDIO or len(frame.payload) < AUDIO_META_LENGTH.size:
            raise WorkerError(ErrorCode.BAD_FRAME, "Malformed audio frame.")
        (meta_length,) = AUDIO_META_LENGTH.unpack_from(frame.payload)
        meta_start = AUDIO_META_LENGTH.size
        meta_end = meta_start + meta_length
        if meta_length > MAX_CONTROL_BYTES or meta_end > len(frame.payload):
            raise WorkerError(ErrorCode.BAD_FRAME, "Invalid audio metadata length.")
        raw_meta = _load_json(frame.payload[meta_start:meta_end], "audio metadata")
        if not isinstance(raw_meta, dict):
            raise WorkerError(ErrorCode.BAD_FRAME, "Audio metadata must be a JSON object.")
        metadata = AudioMetadata.from_dict(raw_meta)
        pcm = frame.payload[meta_end:]
        frame_width = metadata.channels * 2
        if not pcm or len(pcm) % frame_width:
            raise WorkerError(ErrorCode.BAD_FRAME, "PCM payload is empty or not frame-aligned.")
        return AudioFrame(metadata, pcm)


class FrameWriter:
    """Serializes stdout writes from command, job, live, and heartbeat threads."""

    def __init__(self, stream: BinaryIO) -> None:
        self._stream = stream
        self._lock = threading.Lock()

    def write_json(self, data: JsonObject) -> None:
        encoded = encode_json(data)
        with self._lock:
            self._stream.write(encoded)
            self._stream.flush()


def round_trip_json(data: JsonObject) -> JsonObject:
    """Small integration helper used by packaging smoke tests."""
    reader = FrameReader(io.BytesIO(encode_json(data)))
    return reader.parse_json(reader.read())
