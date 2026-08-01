from __future__ import annotations

import io
import struct

import pytest

from local_transcript_worker.errors import ErrorCode, WorkerError
from local_transcript_worker.protocol import (
    HEADER,
    MAGIC,
    FrameKind,
    FrameReader,
    encode_audio,
    encode_json,
)


def test_json_frame_round_trip() -> None:
    reader = FrameReader(io.BytesIO(encode_json({"type": "request", "text": "café"})))

    frame = reader.read()

    assert frame.kind is FrameKind.JSON
    assert reader.parse_json(frame)["text"] == "café"


def test_audio_frame_keeps_pcm_binary() -> None:
    metadata = {
        "session_id": "session-1",
        "stream_id": "microphone",
        "sequence": 3,
        "start_ms": 1000,
        "sample_rate": 48000,
        "channels": 1,
        "sample_format": "s16le",
    }
    pcm = b"\x00\x00\xff\x7f"
    reader = FrameReader(io.BytesIO(encode_audio(metadata, pcm)))

    audio = reader.parse_audio(reader.read())

    assert audio.metadata.sequence == 3
    assert audio.pcm == pcm


def test_rejects_bad_magic() -> None:
    encoded = bytearray(encode_json({"hello": True}))
    encoded[:4] = b"NOPE"

    with pytest.raises(WorkerError) as caught:
        FrameReader(io.BytesIO(encoded)).read()

    assert caught.value.code is ErrorCode.BAD_FRAME


def test_rejects_oversized_length_before_allocating() -> None:
    raw = HEADER.pack(MAGIC, 1, int(FrameKind.JSON), 0, 5 * 1024 * 1024)

    with pytest.raises(WorkerError) as caught:
        FrameReader(io.BytesIO(raw)).read()

    assert caught.value.code is ErrorCode.BAD_FRAME


def test_rejects_truncated_payload() -> None:
    raw = HEADER.pack(MAGIC, 1, int(FrameKind.JSON), 0, 5) + b"{}"

    with pytest.raises(WorkerError) as caught:
        FrameReader(io.BytesIO(raw)).read()

    assert caught.value.code is ErrorCode.BAD_FRAME


def test_audio_metadata_length_is_bounded() -> None:
    payload = struct.pack(">I", 0xFFFFFFFF) + b"{}"
    raw = HEADER.pack(MAGIC, 1, int(FrameKind.AUDIO), 0, len(payload)) + payload
    reader = FrameReader(io.BytesIO(raw))

    with pytest.raises(WorkerError) as caught:
        reader.parse_audio(reader.read())

    assert caught.value.code is ErrorCode.BAD_FRAME


@pytest.mark.parametrize(
    "payload",
    [
        b'{"value":NaN}',
        b'{"duplicate":1,"duplicate":2}',
    ],
)
def test_rejects_nonstandard_or_ambiguous_json(payload: bytes) -> None:
    raw = HEADER.pack(MAGIC, 1, int(FrameKind.JSON), 0, len(payload)) + payload
    reader = FrameReader(io.BytesIO(raw))

    with pytest.raises(WorkerError) as caught:
        reader.parse_json(reader.read())

    assert caught.value.code is ErrorCode.BAD_FRAME
