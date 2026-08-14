#!/usr/bin/env python3
"""Smoke-test the packaged worker protocol without importing project code."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import select
import struct
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any, BinaryIO

HEADER = struct.Struct(">4sBBHQ")
MAGIC = b"LTW1"
PROTOCOL_MAJOR = 1
JSON_KIND = 1
MAX_CONTROL_BYTES = 4 * 1024 * 1024


def frame(data: dict[str, Any]) -> bytes:
    payload = json.dumps(data, separators=(",", ":"), allow_nan=False).encode("utf-8")
    return HEADER.pack(MAGIC, PROTOCOL_MAJOR, JSON_KIND, 0, len(payload)) + payload


def read_exact(stream: BinaryIO, length: int, deadline: float) -> bytes:
    chunks: list[bytes] = []
    remaining = length
    while remaining:
        timeout = deadline - time.monotonic()
        if timeout <= 0 or not select.select([stream], [], [], timeout)[0]:
            raise TimeoutError("Timed out waiting for the packaged worker protocol.")
        chunk = stream.read(remaining)
        if not chunk:
            raise EOFError("Packaged worker closed its protocol output early.")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_message(stream: BinaryIO, deadline: float) -> dict[str, Any]:
    raw_header = read_exact(stream, HEADER.size, deadline)
    magic, major, kind, flags, length = HEADER.unpack(raw_header)
    if magic != MAGIC or major != PROTOCOL_MAJOR or kind != JSON_KIND or flags != 0:
        raise ValueError("Packaged worker emitted an incompatible protocol frame.")
    if length > MAX_CONTROL_BYTES:
        raise ValueError("Packaged worker emitted an oversized protocol frame.")
    value = json.loads(read_exact(stream, length, deadline))
    if not isinstance(value, dict):
        raise TypeError("Packaged worker emitted a non-object control message.")
    return value


def send_message(stream: BinaryIO, value: dict[str, Any]) -> None:
    stream.write(frame(value))
    stream.flush()


def request(request_id: str, command: str, payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "protocol_version": "1.0",
        "type": "request",
        "request_id": request_id,
        "command": command,
        "payload": payload,
    }


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def generate_speech_fixture(ffmpeg: Path, root: Path) -> Path:
    say = Path("/usr/bin/say")
    if not say.is_file():
        raise RuntimeError(
            "macOS speech synthesis is unavailable for inference testing."
        )
    aiff = root / "release-inference.aiff"
    wav = root / "release-inference.wav"
    subprocess.run(
        [
            str(say),
            "-o",
            str(aiff),
            "Say Trace verifies private transcription on Apple silicon.",
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    subprocess.run(
        [
            str(ffmpeg),
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            str(aiff),
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            str(wav),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    return wav.resolve(strict=True)


def verify_model_inference(
    *,
    stdin: BinaryIO,
    stdout: BinaryIO,
    hello: dict[str, Any],
    workspace: Path,
    audio: Path,
    deadline: float,
) -> dict[str, Any]:
    payload = hello.get("payload")
    if not isinstance(payload, dict) or not isinstance(
        payload.get("pipeline_version"), str
    ):
        raise TypeError("Packaged worker hello omitted its pipeline version.")
    pipeline_version = payload["pipeline_version"]
    pipeline_request = request(
        "release-inference",
        "pipeline.run",
        {
            "workspace_path": str(workspace),
            "sources": [
                {
                    "asset_id": "release-audio",
                    "path": str(audio),
                    "source_type": "import",
                }
            ],
            "diarization_asset_id": "release-audio",
            "profiles": [],
            "resume": {},
        },
    )
    pipeline_request["job_id"] = "release-inference"
    pipeline_request["pipeline_version"] = pipeline_version
    send_message(stdin, pipeline_request)

    accepted = False
    mlx_metal_seen = False
    result: dict[str, Any] | None = None
    while result is None or not accepted:
        message = read_message(stdout, deadline)
        if message.get("request_id") == "release-inference":
            if message.get("ok") is not True:
                raise RuntimeError(
                    "Packaged worker rejected the release inference job."
                )
            response = message.get("result")
            accepted = isinstance(response, dict) and response.get("accepted") is True
        if message.get("event") == "performance_timing":
            timing = message.get("payload")
            if (
                isinstance(timing, dict)
                and timing.get("scope") == "model_load"
                and timing.get("component") == "final_asr"
                and timing.get("backend") == "mlx"
                and timing.get("device") == "mps"
            ):
                mlx_metal_seen = True
        if message.get("event") == "job_error":
            event_payload = message.get("payload")
            if isinstance(event_payload, dict) and event_payload.get("job_id") == (
                "release-inference"
            ):
                error = event_payload.get("error")
                code = error.get("code") if isinstance(error, dict) else "unknown"
                detail = (
                    json.dumps(error, sort_keys=True, separators=(",", ":"))
                    if isinstance(error, dict)
                    else repr(error)
                )
                raise RuntimeError(
                    f"Packaged worker inference failed with code {code}: {detail}"
                )
        if message.get("event") == "job_complete":
            event_payload = message.get("payload")
            if isinstance(event_payload, dict) and event_payload.get("job_id") == (
                "release-inference"
            ):
                candidate = event_payload.get("result")
                if not isinstance(candidate, dict):
                    raise TypeError("Packaged worker inference result is malformed.")
                result = candidate

    if not accepted:
        raise ValueError("Packaged worker did not accept the release inference job.")
    if not mlx_metal_seen:
        raise ValueError("Packaged worker did not attest to an MLX/Metal model load.")
    if not isinstance(result.get("word_count"), int) or result["word_count"] <= 0:
        raise ValueError("Packaged worker inference produced no recognized words.")
    artifact_raw = result.get("canonical_artifact_path")
    if not isinstance(artifact_raw, str):
        raise TypeError("Packaged worker inference omitted its canonical artifact.")
    artifact = Path(artifact_raw).resolve(strict=True)
    if (
        not artifact.is_relative_to(workspace.resolve(strict=True))
        or not artifact.is_file()
    ):
        raise ValueError("Packaged worker inference artifact escaped its workspace.")
    artifact_data = json.loads(artifact.read_text(encoding="utf-8"))
    if not isinstance(artifact_data, dict):
        raise TypeError("Packaged worker canonical artifact is malformed.")
    return {
        "backend": "mlx",
        "device": "mps",
        "word_count": result["word_count"],
        "artifact_sha256": sha256(artifact),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--worker", required=True, type=Path)
    parser.add_argument("--ffmpeg", required=True, type=Path)
    parser.add_argument("--timeout-seconds", type=float, default=60.0)
    parser.add_argument("--model-root", type=Path)
    parser.add_argument("--require-model-inference", action="store_true")
    parser.add_argument("--inference-timeout-seconds", type=float, default=900.0)
    args = parser.parse_args()

    worker = args.worker.resolve(strict=True)
    ffmpeg = args.ffmpeg.resolve(strict=True)
    if args.require_model_inference and args.model_root is None:
        parser.error("--require-model-inference requires --model-root")
    model_root = (
        args.model_root.resolve(strict=True) if args.model_root is not None else None
    )
    if model_root is not None and not model_root.is_dir():
        parser.error("--model-root must be a directory")
    with (
        tempfile.TemporaryDirectory(prefix="saytrace-worker-smoke-") as temporary,
        tempfile.TemporaryFile() as stderr_stream,
    ):
        root = Path(temporary)
        selected_model_root = model_root or root / "models"
        allowed_root = root / "library"
        if model_root is None:
            selected_model_root.mkdir()
        allowed_root.mkdir()
        command = [
            str(worker),
            "--model-root",
            str(selected_model_root),
            "--allowed-root",
            str(allowed_root),
            "--ffmpeg",
            str(ffmpeg),
            "--heartbeat-seconds",
            "3600",
        ]
        worker_environment = os.environ.copy()
        worker_environment["SAYTRACE_RELEASE_DIAGNOSTICS"] = "1"
        process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            # Native frameworks can log enough during first-run cache creation
            # to fill a pipe. A temporary file keeps stderr separate from the
            # binary protocol without ever back-pressuring inference.
            stderr=stderr_stream,
            env=worker_environment,
            bufsize=0,
        )
        assert process.stdin is not None
        assert process.stdout is not None
        deadline = time.monotonic() + args.timeout_seconds
        try:
            hello = read_message(process.stdout, deadline)
            if hello.get("event") != "hello":
                raise ValueError("Packaged worker did not emit its hello event first.")
            if hello.get("protocol_version") != "1.0":
                raise ValueError(
                    "Packaged worker hello used the wrong protocol version."
                )
            send_message(process.stdin, request("release-ping", "ping", {}))
            ping = read_message(process.stdout, time.monotonic() + args.timeout_seconds)
            if ping.get("request_id") != "release-ping" or not (
                isinstance(ping.get("result"), dict)
                and ping["result"].get("pong") is True
            ):
                raise ValueError("Packaged worker ping failed.")

            inference: dict[str, Any] | None = None
            if args.require_model_inference:
                audio = generate_speech_fixture(ffmpeg, allowed_root)
                inference = verify_model_inference(
                    stdin=process.stdin,
                    stdout=process.stdout,
                    hello=hello,
                    workspace=allowed_root,
                    audio=audio,
                    deadline=time.monotonic() + args.inference_timeout_seconds,
                )

            send_message(process.stdin, request("release-shutdown", "shutdown", {}))
            shutdown = read_message(
                process.stdout, time.monotonic() + args.timeout_seconds
            )
            if shutdown.get("request_id") != "release-shutdown":
                raise ValueError("Packaged worker shutdown response was missing.")
            process.stdin.close()
            if process.wait(timeout=args.timeout_seconds) != 0:
                raise RuntimeError("Packaged worker exited unsuccessfully.")
        # Always reap the subprocess and surface its native diagnostics before
        # propagating any protocol, inference, I/O, or decoding failure.
        except Exception as cause:  # noqa: BLE001
            # Closing stdin asks a healthy worker to take its normal EOF cleanup
            # path. Its job manager can require up to ten seconds to join.
            if process.stdin is not None and not process.stdin.closed:
                try:
                    process.stdin.close()
                except (BrokenPipeError, OSError):
                    pass
            observed_return_code = process.poll()
            if observed_return_code is None:
                try:
                    return_code = process.wait(timeout=12)
                    status_detail = (
                        f"worker exited on its own with status {return_code}"
                    )
                except subprocess.TimeoutExpired:
                    process.kill()
                    return_code = process.wait(timeout=10)
                    status_detail = f"terminated by verifier with status {return_code}"
            else:
                return_code = observed_return_code
                status_detail = f"worker exited on its own with status {return_code}"
            stderr_stream.seek(0, 2)
            stderr_length = stderr_stream.tell()
            stderr_stream.seek(max(0, stderr_length - 2000))
            stderr = stderr_stream.read().decode("utf-8", errors="replace")
            cause_detail = f"{type(cause).__name__}: {cause}"
            if stderr:
                raise RuntimeError(
                    "Packaged worker smoke test failed "
                    f"({status_detail}; {cause_detail}): {stderr}"
                ) from None
            raise RuntimeError(
                "Packaged worker smoke test failed without diagnostic output "
                f"({status_detail}; {cause_detail}). Check macOS DiagnosticReports for "
                "a code-signing or native crash report."
            ) from None

    if inference is None:
        print("Packaged worker hello, ping, and shutdown passed.")
    else:
        print(
            "Packaged worker hello, ping, MLX/Metal inference, and shutdown passed "
            f"({inference['word_count']} words)."
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
