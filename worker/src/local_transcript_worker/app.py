"""Command dispatch and worker composition."""

from __future__ import annotations

import importlib.util
import os
import platform
import threading
import time
from collections.abc import Mapping
from pathlib import Path
from typing import Any

from . import PROTOCOL_VERSION, __version__
from .backends import (
    FasterWhisperBackend,
    FfmpegNormalizer,
    PyannoteDiarizer,
    RetryingAligner,
    RetryingDiarizer,
    RetryingEmbedder,
    RetryingTranscriber,
    WeSpeakerEmbedder,
    WhisperXAligner,
)
from .environment import ApprovedPaths
from .errors import ErrorCode, WorkerError
from .jobs import JobManager
from .live import LiveDraftManager
from .models import ModelManifest, ModelStore
from .pipeline import FinalPipeline, PipelineInput
from .profiles import MatchPolicy, VoiceProfile
from .protocol import AudioFrame, FrameWriter
from .schema import (
    JsonObject,
    PipelineCheckpoint,
    Request,
    SourceAsset,
    require_identifier,
    require_object,
    require_string,
)


class EventEmitter:
    def __init__(self, writer: FrameWriter) -> None:
        self.writer = writer
        self._sequence = 0
        self._lock = threading.Lock()

    def emit(self, event: str, payload: JsonObject) -> None:
        self._write(
            {
                "protocol_version": PROTOCOL_VERSION,
                "type": "event",
                "event": event,
                "payload": payload,
            }
        )

    def response(self, request_id: str, result: JsonObject) -> None:
        self._write(
            {
                "protocol_version": PROTOCOL_VERSION,
                "type": "response",
                "request_id": request_id,
                "ok": True,
                "result": result,
            }
        )

    def error(self, request_id: str | None, error: WorkerError) -> None:
        self._write(
            {
                "protocol_version": PROTOCOL_VERSION,
                "type": "response",
                "request_id": request_id,
                "ok": False,
                "error": error.as_dict(),
            }
        )

    def _write(self, message: JsonObject) -> None:
        # Hold this lock through the framed write so on-wire ordering matches sequence order.
        with self._lock:
            self._sequence += 1
            message["sequence"] = self._sequence
            message["timestamp_ms"] = int(time.time() * 1000)
            self.writer.write_json(message)


class WorkerApp:
    def __init__(
        self,
        writer: FrameWriter,
        *,
        model_root: Path,
        approved_roots: list[Path],
        ffmpeg_path: Path,
        setup_enabled: bool,
        heartbeat_seconds: float = 10.0,
    ) -> None:
        self.emitter = EventEmitter(writer)
        self.manifest = ModelManifest.load()
        self.models = ModelStore(model_root, self.manifest, setup_enabled=setup_enabled)
        self.approved_paths = ApprovedPaths(approved_roots)
        self.ffmpeg_path = ffmpeg_path.resolve(strict=True)
        if not self.ffmpeg_path.is_file():
            raise WorkerError(ErrorCode.INVALID_PATH, "FFmpeg path is not a file.")
        if heartbeat_seconds < 0.25:
            raise WorkerError(
                ErrorCode.BAD_REQUEST, "Heartbeat interval must be at least 0.25 seconds."
            )
        self.setup_enabled = setup_enabled
        self.jobs = JobManager(self.emitter.emit)
        self.live: LiveDraftManager | None = None
        self._shutdown = threading.Event()
        self._heartbeat_seconds = heartbeat_seconds
        self._heartbeat = threading.Thread(
            target=self._heartbeat_loop, name="worker-heartbeat", daemon=True
        )
        self._heartbeat.start()

    @property
    def shutting_down(self) -> bool:
        return self._shutdown.is_set()

    def hello(self) -> None:
        self.emitter.emit(
            "hello",
            {
                "worker_version": __version__,
                "protocol_version": PROTOCOL_VERSION,
                "pipeline_version": self.manifest.pipeline_version,
                "python_version": platform.python_version(),
                "pid": os.getpid(),
                "setup_enabled": self.setup_enabled,
                "network_mode": "model-setup" if self.setup_enabled else "blocked",
                "capabilities": [
                    "health",
                    "model_setup",
                    "live_draft",
                    "final_transcription",
                    "word_alignment",
                    "exclusive_diarization",
                    "speaker_profiles",
                ],
            },
        )

    def handle_request(self, request: Request) -> JsonObject:
        command = request.command
        payload = request.payload
        if command == "ping":
            return {"pong": True, "monotonic_ms": round(time.monotonic() * 1000)}
        if command == "health":
            return self._health()
        if command == "model.status":
            return self.models.status()
        if command == "model.verify":
            return self.models.verify(require_string(payload.get("key"), "key"))
        if command == "model.install":
            return self._model_install(request)
        if command == "live.start":
            return self._live_start(payload)
        if command == "live.stop":
            return self._live_stop(payload)
        if command == "pipeline.run":
            return self._pipeline_run(request)
        if command == "pipeline.cancel":
            job_id = require_identifier(payload.get("job_id"), "job_id")
            return {"job_id": job_id, "cancel_requested": self.jobs.cancel(job_id)}
        if command == "shutdown":
            self._shutdown.set()
            return {"shutting_down": True}
        raise WorkerError(ErrorCode.BAD_REQUEST, f"Unknown command {command!r}.")

    def handle_audio(self, frame: AudioFrame) -> None:
        if self.live is None:
            raise WorkerError(ErrorCode.BAD_REQUEST, "No live caption session is active.")
        self.live.push(frame.metadata, frame.pcm)

    def close(self) -> None:
        self._shutdown.set()
        if self.live:
            self.live.close()
            self.live = None
        self.jobs.close()
        self._heartbeat.join(timeout=2)

    def _health(self) -> JsonObject:
        packages = {
            name: self._package_available(name)
            for name in ("faster_whisper", "whisperx", "pyannote.audio", "torch")
        }
        ctranslate_gpu = False
        try:
            import ctranslate2

            count = int(ctranslate2.get_cuda_device_count())
            ctranslate_gpu = count > 0
        except (ImportError, RuntimeError):
            count = 0
        torch_gpu = False
        try:
            import torch

            torch_gpu = bool(torch.cuda.is_available())
        except (ImportError, RuntimeError):
            pass
        return {
            "status": "ok",
            "worker_version": __version__,
            "pipeline_version": self.manifest.pipeline_version,
            "packages": packages,
            "gpu": {
                "available": ctranslate_gpu or torch_gpu,
                "ctranslate2_cuda": ctranslate_gpu,
                "ctranslate2_device_count": count,
                "torch_cuda": torch_gpu,
            },
            "models": self.models.status(verify_hashes=False),
            "jobs": self.jobs.status(),
            "network_mode": "model-setup" if self.setup_enabled else "blocked",
        }

    def _live_start(self, payload: Mapping[str, Any]) -> JsonObject:
        session_id = require_identifier(payload.get("session_id"), "session_id")
        raw_streams = require_object(payload.get("streams"), "streams")
        stream_types = {
            require_identifier(stream_id, "stream_id"): require_string(source_type, "source_type")
            for stream_id, source_type in raw_streams.items()
        }
        if not stream_types:
            raise WorkerError(
                ErrorCode.BAD_REQUEST, "Live captions require at least one audio stream."
            )
        if self.live is not None and self.live.active_session_count == 0:
            if not self.live.close():
                raise WorkerError(
                    ErrorCode.WORKER_BUSY,
                    "The prior live caption model is still being released.",
                    retryable=True,
                )
            self.live = None
        if self.live is None:
            path = self.models.require("live_asr_en")
            device = self._preferred_asr_device()
            compute_type = "float16" if device == "cuda" else "int8"
            backend = FasterWhisperBackend(
                path,
                device=device,
                compute_type=compute_type,
                beam_size=1,
                vad_filter=True,
            )
            self.live = LiveDraftManager(backend, self.emitter.emit)
        self.live.start_session(session_id, stream_types)
        return {"session_id": session_id, "state": "started", "streams": stream_types}

    def _live_stop(self, payload: Mapping[str, Any]) -> JsonObject:
        session_id = require_identifier(payload.get("session_id"), "session_id")
        if self.live is None:
            raise WorkerError(ErrorCode.BAD_REQUEST, "No live caption session is active.")
        released = self.live.stop_session(session_id)
        if released:
            self.live = None
        return {
            "session_id": session_id,
            "state": "stopped",
            "live_model_released": released,
        }

    def _model_install(self, request: Request) -> JsonObject:
        payload = request.payload
        key = require_string(payload.get("key"), "key")
        token = payload.get("token")

        def progress(phase: str, completed: int, total: int) -> None:
            self.emitter.emit(
                "model_setup_progress",
                {
                    "request_id": request.request_id,
                    "key": key,
                    "code": "MODEL_SETUP_PROGRESS",
                    "phase": phase,
                    "completed_steps": completed,
                    "total_steps": total,
                },
            )

        try:
            return self.models.install(
                key,
                token=require_string(token, "token") if token is not None else None,
                progress=progress,
            )
        except WorkerError as exc:
            self.emitter.emit(
                "model_setup_progress",
                {
                    "request_id": request.request_id,
                    "key": key,
                    "code": exc.code.value,
                    "phase": "failed",
                    "completed_steps": 0,
                    "total_steps": 4,
                    "retryable": exc.retryable,
                },
            )
            raise
        finally:
            payload.pop("token", None)

    def _pipeline_run(self, request: Request) -> JsonObject:
        if not request.job_id:
            raise WorkerError(ErrorCode.BAD_REQUEST, "pipeline.run requires 'job_id'.")
        if request.pipeline_version != self.manifest.pipeline_version:
            raise WorkerError(
                ErrorCode.PROTOCOL_MISMATCH,
                "Requested pipeline version does not match installed worker assets.",
                {
                    "requested": request.pipeline_version,
                    "installed": self.manifest.pipeline_version,
                },
            )
        pipeline_input = self._parse_pipeline_input(request)
        if self.live is not None:
            active_sessions = self.live.active_session_count
            if active_sessions:
                raise WorkerError(
                    ErrorCode.WORKER_BUSY,
                    "Stop live captions before starting final processing.",
                    {"active_live_sessions": active_sessions},
                    retryable=True,
                )
            if not self.live.close():
                raise WorkerError(
                    ErrorCode.WORKER_BUSY,
                    "The live caption CUDA model is still being released.",
                    {"active_live_sessions": 0},
                    retryable=True,
                )
            self.live = None

        def run(cancel: threading.Event) -> JsonObject:
            pipeline = self._create_pipeline(pipeline_input)
            return pipeline.run(pipeline_input, cancel)

        self.jobs.submit(request.job_id, run)
        return {
            "job_id": request.job_id,
            "accepted": True,
            "pipeline_version": request.pipeline_version,
        }

    def _parse_pipeline_input(self, request: Request) -> PipelineInput:
        payload = request.payload
        workspace = self.approved_paths.resolve_existing(
            require_string(payload.get("workspace_path"), "workspace_path"),
            kind="directory",
        )
        raw_sources = payload.get("sources")
        if not isinstance(raw_sources, list) or not raw_sources:
            raise WorkerError(ErrorCode.BAD_REQUEST, "'sources' must be a non-empty array.")
        sources: list[SourceAsset] = []
        for raw in raw_sources:
            data = require_object(raw, "source")
            path = self.approved_paths.resolve_existing(
                require_string(data.get("path"), "path"), kind="file"
            )
            sources.append(SourceAsset.from_dict(data, path))
        diarization_id = require_string(payload.get("diarization_asset_id"), "diarization_asset_id")
        if diarization_id not in {source.asset_id for source in sources}:
            raise WorkerError(
                ErrorCode.BAD_REQUEST, "diarization_asset_id is not in the source list."
            )
        raw_profiles = payload.get("profiles", [])
        if not isinstance(raw_profiles, list):
            raise WorkerError(ErrorCode.BAD_REQUEST, "'profiles' must be an array.")
        profiles = tuple(
            VoiceProfile.from_dict(require_object(value, "profile")) for value in raw_profiles
        )
        raw_policy = payload.get("match_policy")
        policy = (
            MatchPolicy.from_dict(require_object(raw_policy, "match_policy"))
            if raw_policy is not None
            else None
        )
        raw_resume = payload.get("resume", {})
        checkpoint = PipelineCheckpoint.from_dict(
            require_object(raw_resume, "resume"), self.manifest.pipeline_version
        )
        return PipelineInput(
            job_id=request.job_id or "",
            pipeline_version=self.manifest.pipeline_version,
            sources=tuple(sources),
            workspace=workspace,
            diarization_asset_id=diarization_id,
            profiles=profiles,
            match_policy=policy,
            checkpoint=checkpoint,
        )

    def _create_pipeline(self, pipeline_input: PipelineInput) -> FinalPipeline:
        final_model = self.models.require("final_asr_en")
        asr_device = self._preferred_asr_device()
        torch_device = self._preferred_torch_device()

        def report_fallback(stage: str, error: WorkerError) -> None:
            self.emitter.emit(
                "job_progress",
                {
                    "job_id": pipeline_input.job_id,
                    "pipeline_version": self.manifest.pipeline_version,
                    "stage": stage,
                    "status": "retrying_cpu",
                    "completed_batches": 0,
                    "total_batches": 1,
                    "resume": pipeline_input.checkpoint.as_dict(),
                    "code": "GPU_OOM_CPU_FALLBACK",
                    "source_error_code": error.code.value,
                    "retryable": True,
                },
            )

        if asr_device == "cuda":
            transcriber = RetryingTranscriber(
                [
                    FasterWhisperBackend(
                        final_model,
                        device="cuda",
                        compute_type="float16",
                        beam_size=5,
                        batch_size=8,
                    ),
                    FasterWhisperBackend(
                        final_model,
                        device="cuda",
                        compute_type="float16",
                        beam_size=5,
                        batch_size=2,
                    ),
                    FasterWhisperBackend(
                        final_model,
                        device="cuda",
                        compute_type="int8_float16",
                        beam_size=5,
                        batch_size=2,
                    ),
                    FasterWhisperBackend(
                        final_model, device="cpu", compute_type="int8", beam_size=5
                    ),
                ]
            )
        else:
            transcriber = RetryingTranscriber(
                [FasterWhisperBackend(final_model, device="cpu", compute_type="int8", beam_size=5)]
            )
        alignment_model = self.models.require("alignment_en")
        diarization_model = self.models.require("diarization")
        embedding_model = self.models.require("speaker_embedding")
        if torch_device == "cuda":
            aligner = RetryingAligner(
                [
                    WhisperXAligner(alignment_model, device="cuda"),
                    WhisperXAligner(alignment_model, device="cpu"),
                ],
                on_fallback=report_fallback,
            )
            diarizer = RetryingDiarizer(
                [
                    PyannoteDiarizer(diarization_model, device="cuda"),
                    PyannoteDiarizer(diarization_model, device="cpu"),
                ],
                on_fallback=report_fallback,
            )
            embedder = RetryingEmbedder(
                [
                    WeSpeakerEmbedder(embedding_model, device="cuda"),
                    WeSpeakerEmbedder(embedding_model, device="cpu"),
                ],
                on_fallback=report_fallback,
            )
        else:
            aligner = RetryingAligner([WhisperXAligner(alignment_model, device="cpu")])
            diarizer = RetryingDiarizer([PyannoteDiarizer(diarization_model, device="cpu")])
            embedder = RetryingEmbedder([WeSpeakerEmbedder(embedding_model, device="cpu")])
        return FinalPipeline(
            normalizer=FfmpegNormalizer(self.ffmpeg_path),
            transcriber=transcriber,
            aligner=aligner,
            diarizer=diarizer,
            embedder=embedder,
            emit=self.emitter.emit,
        )

    @staticmethod
    def _preferred_asr_device() -> str:
        try:
            import ctranslate2

            return "cuda" if ctranslate2.get_cuda_device_count() > 0 else "cpu"
        except (ImportError, RuntimeError):
            return "cpu"

    @staticmethod
    def _preferred_torch_device() -> str:
        try:
            import torch

            return "cuda" if torch.cuda.is_available() else "cpu"
        except (ImportError, RuntimeError):
            return "cpu"

    def _heartbeat_loop(self) -> None:
        while not self._shutdown.wait(self._heartbeat_seconds):
            self.emitter.emit(
                "heartbeat",
                {
                    "monotonic_ms": round(time.monotonic() * 1000),
                    "jobs": self.jobs.status(),
                },
            )

    @staticmethod
    def _package_available(name: str) -> bool:
        try:
            return importlib.util.find_spec(name) is not None
        except (ImportError, ModuleNotFoundError):
            return False


def unhandled_error(exc: Exception) -> WorkerError:
    if isinstance(exc, WorkerError):
        return exc
    return WorkerError(
        ErrorCode.INTERNAL,
        "Unhandled worker error.",
        {"exception": type(exc).__name__},
    )
