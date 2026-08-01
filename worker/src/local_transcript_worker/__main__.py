"""Executable sidecar entrypoint."""

from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path
from typing import BinaryIO

from . import __version__
from .app import WorkerApp, unhandled_error
from .environment import enforce_offline_environment
from .errors import ErrorCode, WorkerError
from .protocol import FrameKind, FrameReader, FrameWriter
from .schema import Request


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="local-transcript-worker")
    parser.add_argument("--version", action="version", version=__version__)
    parser.add_argument("--model-root", required=True, type=Path)
    parser.add_argument("--allowed-root", required=True, action="append", type=Path)
    parser.add_argument("--ffmpeg", required=True, type=Path)
    parser.add_argument("--heartbeat-seconds", type=float, default=10.0)
    parser.add_argument(
        "--allow-model-downloads",
        action="store_true",
        help="Enable one-time model setup. Never use for normal inference.",
    )
    return parser


def run(
    input_stream: BinaryIO,
    output_stream: BinaryIO,
    *,
    model_root: Path,
    allowed_roots: list[Path],
    ffmpeg_path: Path,
    allow_model_downloads: bool,
    heartbeat_seconds: float,
) -> int:
    if not allow_model_downloads:
        enforce_offline_environment(install_socket_guard=True)
    writer = FrameWriter(output_stream)
    try:
        app = WorkerApp(
            writer,
            model_root=model_root,
            approved_roots=allowed_roots,
            ffmpeg_path=ffmpeg_path,
            setup_enabled=allow_model_downloads,
            heartbeat_seconds=heartbeat_seconds,
        )
    except Exception as exc:
        error = unhandled_error(exc)
        writer.write_json(
            {
                "protocol_version": "1.0",
                "type": "fatal",
                "sequence": 0,
                "error": error.as_dict(),
            }
        )
        return 2

    reader = FrameReader(input_stream)
    app.hello()
    try:
        while not app.shutting_down:
            try:
                frame = reader.read()
                if frame.kind is FrameKind.JSON:
                    raw = reader.parse_json(frame)
                    request_id = raw.get("request_id")
                    try:
                        request = Request.from_dict(raw)
                        result = app.handle_request(request)
                        app.emitter.response(request.request_id, result)
                    except Exception as exc:
                        app.emitter.error(
                            request_id if isinstance(request_id, str) else None,
                            unhandled_error(exc),
                        )
                else:
                    try:
                        app.handle_audio(reader.parse_audio(frame))
                    except Exception as exc:
                        app.emitter.emit("audio_error", {"error": unhandled_error(exc).as_dict()})
            except EOFError:
                break
            except WorkerError as exc:
                app.emitter.error(None, exc)
                if exc.code is ErrorCode.BAD_FRAME:
                    break
    finally:
        app.close()
    return 0


def main() -> int:
    logging.basicConfig(
        level=logging.INFO,
        stream=sys.stderr,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    args = build_parser().parse_args()
    return run(
        sys.stdin.buffer,
        sys.stdout.buffer,
        model_root=args.model_root,
        allowed_roots=args.allowed_root,
        ffmpeg_path=args.ffmpeg,
        allow_model_downloads=args.allow_model_downloads,
        heartbeat_seconds=args.heartbeat_seconds,
    )


if __name__ == "__main__":
    raise SystemExit(main())
