from __future__ import annotations

import io
from pathlib import Path

from local_transcript_worker.__main__ import run
from local_transcript_worker.protocol import FrameReader, encode_json


def test_sidecar_hello_ping_shutdown_without_ml_dependencies(tmp_path: Path) -> None:
    ffmpeg = tmp_path / "ffmpeg.exe"
    ffmpeg.write_bytes(b"test")
    model_root = tmp_path / "models"
    allowed = tmp_path / "library"
    allowed.mkdir()
    request_stream = io.BytesIO(
        encode_json(
            {
                "protocol_version": "1.0",
                "type": "request",
                "request_id": "ping-1",
                "command": "ping",
                "payload": {},
            }
        )
        + encode_json(
            {
                "protocol_version": "1.0",
                "type": "request",
                "request_id": "shutdown-1",
                "command": "shutdown",
                "payload": {},
            }
        )
    )
    output = io.BytesIO()

    exit_code = run(
        request_stream,
        output,
        model_root=model_root,
        allowed_roots=[allowed],
        ffmpeg_path=ffmpeg,
        allow_model_downloads=True,
        heartbeat_seconds=3600,
    )

    assert exit_code == 0
    output.seek(0)
    reader = FrameReader(output)
    messages = [reader.parse_json(reader.read()) for _ in range(3)]
    assert [message["sequence"] for message in messages] == [1, 2, 3]
    assert messages[0]["event"] == "hello"
    assert messages[0]["protocol_version"] == "1.0"
    assert messages[0]["payload"]["protocol_version"] == "1.0"
    assert messages[0]["payload"]["pipeline_version"] == "2026.07.28.1"
    assert messages[0]["payload"]["setup_enabled"] is True
    assert messages[1]["request_id"] == "ping-1"
    assert messages[1]["result"]["pong"] is True
    assert messages[2]["request_id"] == "shutdown-1"
