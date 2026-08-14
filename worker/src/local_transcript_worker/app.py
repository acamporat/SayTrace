"""Command dispatch and worker composition."""

from __future__ import annotations

import importlib.util
import os
import platform
import threading
import time
from collections.abc import Callable, Mapping, Sequence
from functools import partial
from pathlib import Path
from typing import Any, cast

from . import PROTOCOL_VERSION, __version__
from .backends import (
    Aligner,
    Diarizer,
    Embedder,
    FasterWhisperBackend,
    FfmpegNormalizer,
    LiveTranscriber,
    MlxWhisperBackend,
    ModelLoadCallback,
    NativeWordTimestampAligner,
    PyannoteDiarizer,
    RetryingAligner,
    RetryingDiarizer,
    RetryingEmbedder,
    RetryingTranscriber,
    Transcriber,
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
from .resident import BackendLease, ResidentBackendCache, resident_cache_policy
from .schema import (
    JsonObject,
    PipelineCheckpoint,
    Request,
    SourceAsset,
    require_identifier,
    require_object,
    require_string,
)

_PREWARM_COMPONENTS = frozenset({"final_asr", "diarization", "speaker_embedding"})


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
        model_cache_idle_seconds: float | None = None,
        model_cache_max_entries: int | None = None,
        physical_memory_bytes_override: int | None = None,
        resident_models_enabled_override: bool | None = None,
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
        self._resident_models_enabled = (
            self._uses_mlx_asr()
            if resident_models_enabled_override is None
            else resident_models_enabled_override
        )
        self._resident_policy = resident_cache_policy(
            enabled=self._resident_models_enabled,
            detected_memory_bytes=physical_memory_bytes_override,
            idle_seconds=model_cache_idle_seconds,
            max_entries=model_cache_max_entries,
        )
        self._compute_lock = threading.RLock()
        self._resident = ResidentBackendCache(
            idle_seconds=self._resident_policy.idle_seconds,
            max_entries=self._resident_policy.max_entries,
            lifecycle_lock=self._compute_lock,
            start_reaper=self._resident_models_enabled,
        )
        self._prewarm_lock = threading.Lock()
        self._prewarm_thread: threading.Thread | None = None
        self._prewarm_request_id: str | None = None
        self._prewarm_components: tuple[str, ...] = ()
        self._prewarm_skipped_components: tuple[str, ...] = ()
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
                    *(
                        ["resident_model_cache", "model_prewarm"]
                        if self._resident_models_enabled
                        else []
                    ),
                    "apple_mlx_asr" if self._uses_mlx_asr() else "ctranslate2_asr",
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
        if command == "performance.prewarm":
            return self._performance_prewarm(request)
        if command == "performance.release":
            return self._performance_release(request.payload)
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
        with self._prewarm_lock:
            prewarm = self._prewarm_thread
        if prewarm is not None:
            prewarm.join(timeout=30)
        self._resident.close()
        self._heartbeat.join(timeout=2)

    def _health(self) -> JsonObject:
        packages = {
            name: self._package_available(name)
            for name in (
                "mlx",
                "mlx_whisper",
                "faster_whisper",
                "whisperx",
                "pyannote.audio",
                "torch",
            )
        }
        ctranslate_gpu = False
        try:
            import ctranslate2

            count = int(ctranslate2.get_cuda_device_count())
            ctranslate_gpu = count > 0
        except (ImportError, RuntimeError):
            count = 0
        torch_gpu = False
        torch_mps = False
        try:
            import torch

            torch_gpu = bool(torch.cuda.is_available())
            torch_mps = bool(hasattr(torch.backends, "mps") and torch.backends.mps.is_available())
        except (ImportError, RuntimeError):
            pass
        mlx_available = self._uses_mlx_asr() and packages["mlx"] and packages["mlx_whisper"]
        return {
            "status": "ok",
            "worker_version": __version__,
            "pipeline_version": self.manifest.pipeline_version,
            "packages": packages,
            "gpu": {
                "available": ctranslate_gpu or torch_gpu or torch_mps or mlx_available,
                "ctranslate2_cuda": ctranslate_gpu,
                "ctranslate2_device_count": count,
                "torch_cuda": torch_gpu,
                "torch_mps": torch_mps,
                "apple_mlx": mlx_available,
            },
            "accelerator": {
                "backend": (
                    "mlx"
                    if mlx_available
                    else "cuda"
                    if ctranslate_gpu or torch_gpu
                    else "mps"
                    if torch_mps
                    else "cpu"
                ),
                "available": mlx_available or ctranslate_gpu or torch_gpu or torch_mps,
            },
            "models": self.models.status(verify_hashes=False),
            "performance": {
                "resident_models_enabled": self._resident_models_enabled,
                "resident_model_policy": self._resident_policy.as_dict(),
                "resident_models": self._resident.status(),
                "prewarm": self._prewarm_status(),
            },
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
        active_jobs = self.jobs.status()["active_job_ids"]
        prewarm_running = bool(self._prewarm_status()["running"])
        if active_jobs or prewarm_running:
            # Recording capture is owned by Rust and remains lossless. Live
            # captions are disposable, so do not let their process-global MLX
            # model replace a final/prewarm model already using the accelerator.
            raise WorkerError(
                ErrorCode.WORKER_BUSY,
                "Live captions are unavailable while final inference is active.",
                {
                    "active_final_jobs": len(active_jobs),
                    "prewarm_running": prewarm_running,
                },
                retryable=True,
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
            # mlx-whisper has one process-global model holder. A live-caption
            # model replaces the final model, so release idle final resources up
            # front and report the cache state truthfully. Never make recording
            # startup wait behind an already-running final pipeline/prewarm.
            if (
                self._resident_models_enabled
                and not self.jobs.status()["active_job_ids"]
                and not self._prewarm_status()["running"]
            ):
                self._resident.evict_idle(force=True)
            path = self.models.require("live_asr_en")
            backend: LiveTranscriber
            if self._uses_mlx_asr():
                backend = MlxWhisperBackend(
                    path,
                    beam_size=1,
                    condition_on_previous_text=False,
                    word_timestamps=False,
                    on_model_load=self._model_load_callback("mlx", "mps"),
                    component="live_asr",
                )
            else:
                device = self._preferred_asr_device()
                compute_type = "float16" if device == "cuda" else "int8"
                backend = FasterWhisperBackend(
                    path,
                    device=device,
                    compute_type=compute_type,
                    beam_size=1,
                    vad_filter=True,
                    on_model_load=self._model_load_callback("ctranslate2", device),
                    component="live_asr",
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

    def _performance_prewarm(self, request: Request) -> JsonObject:
        if not self._resident_models_enabled:
            raise WorkerError(
                ErrorCode.BAD_REQUEST,
                "Resident model prewarm is available only on Apple Silicon.",
            )
        requested_components = self._parse_performance_components(
            request.payload, default=("final_asr", "diarization")
        )
        allowed_components = set(self._resident_policy.prewarm_components)
        components = tuple(
            component for component in requested_components if component in allowed_components
        )
        skipped_components = tuple(
            component for component in requested_components if component not in allowed_components
        )
        if not components:
            self.emitter.emit(
                "performance_prewarm_complete",
                {
                    "request_id": request.request_id,
                    "components": [],
                    "skipped_components": list(skipped_components),
                },
            )
            return {
                "accepted": False,
                "state": "skipped_by_memory_policy",
                "request_id": request.request_id,
                "components": [],
                "skipped_components": list(skipped_components),
            }
        if self.live is not None and self.live.active_session_count:
            raise WorkerError(
                ErrorCode.WORKER_BUSY,
                "Final models cannot be prewarmed while live captions are active.",
                retryable=True,
            )
        job_status = self.jobs.status()
        if job_status["active_job_ids"]:
            raise WorkerError(
                ErrorCode.WORKER_BUSY,
                "Final models cannot be prewarmed while a processing job is active.",
                retryable=True,
            )
        with self._prewarm_lock:
            if self._prewarm_thread is not None and self._prewarm_thread.is_alive():
                result: JsonObject = {
                    "accepted": False,
                    "state": "already_running",
                    "request_id": self._prewarm_request_id,
                    "components": list(self._prewarm_components),
                }
                if self._prewarm_skipped_components:
                    result["skipped_components"] = list(self._prewarm_skipped_components)
                return result
            self._prewarm_request_id = request.request_id
            self._prewarm_components = components
            self._prewarm_skipped_components = skipped_components
            self._prewarm_thread = threading.Thread(
                target=self._run_prewarm,
                args=(request.request_id, components, skipped_components),
                name="model-prewarm",
                daemon=True,
            )
            self._prewarm_thread.start()
        result = {
            "accepted": True,
            "state": "warming",
            "request_id": request.request_id,
            "components": list(components),
        }
        if skipped_components:
            result["skipped_components"] = list(skipped_components)
        return result

    def _run_prewarm(
        self,
        request_id: str,
        components: tuple[str, ...],
        skipped_components: tuple[str, ...],
    ) -> None:
        completed: list[str] = []
        started_payload: JsonObject = {
            "request_id": request_id,
            "components": list(components),
        }
        if skipped_components:
            started_payload["skipped_components"] = list(skipped_components)
        self.emitter.emit(
            "performance_prewarm_started",
            started_payload,
        )
        try:
            with self._compute_lock:
                for component in components:
                    if self._shutdown.is_set():
                        raise WorkerError(ErrorCode.CANCELLED, "Model prewarm was cancelled.")
                    started = time.monotonic()
                    was_resident = any(
                        key.split(":", 1)[0] == component for key in self._resident.keys()
                    )
                    leases: list[BackendLease[object]] = []
                    try:
                        backend = self._prewarm_backend(component, leases)
                        prewarm = getattr(backend, "prewarm", None)
                        if not callable(prewarm):
                            raise WorkerError(
                                ErrorCode.INTERNAL,
                                "The selected backend does not support prewarming.",
                                {"component": component},
                            )
                        prewarm()
                    finally:
                        for lease in reversed(leases):
                            lease.close()
                    completed.append(component)
                    self.emitter.emit(
                        "performance_timing",
                        {
                            "scope": "model_prewarm",
                            "request_id": request_id,
                            "component": component,
                            "duration_ms": max(0, round((time.monotonic() - started) * 1000)),
                            "cache_hit": was_resident,
                        },
                    )
            complete_payload: JsonObject = {
                "request_id": request_id,
                "components": completed,
            }
            if skipped_components:
                complete_payload["skipped_components"] = list(skipped_components)
            self.emitter.emit("performance_prewarm_complete", complete_payload)
        except Exception as exc:
            error_payload: JsonObject = {
                "request_id": request_id,
                "components": completed,
                "error": unhandled_error(exc).as_dict(),
            }
            if skipped_components:
                error_payload["skipped_components"] = list(skipped_components)
            self.emitter.emit("performance_prewarm_error", error_payload)
        finally:
            with self._prewarm_lock:
                if self._prewarm_request_id == request_id:
                    self._prewarm_request_id = None
                    self._prewarm_components = ()
                    self._prewarm_skipped_components = ()

    def _prewarm_backend(self, component: str, leases: list[BackendLease[object]]) -> object:
        if component == "final_asr":
            asr_attempts = self._final_asr_attempts(
                self.models.require("final_asr_en"),
                self._preferred_asr_device(),
                leases,
            )
            return asr_attempts[0]
        if component == "diarization":
            diarization_attempts = self._diarization_attempts(
                self.models.require("diarization"),
                self._preferred_torch_device(),
                leases,
            )
            return diarization_attempts[0]
        embedding_attempts = self._embedding_attempts(
            self.models.require("speaker_embedding"),
            self._preferred_torch_device(),
            leases,
        )
        return embedding_attempts[0]

    def _performance_release(self, payload: Mapping[str, Any]) -> JsonObject:
        if not self._resident_models_enabled:
            raise WorkerError(
                ErrorCode.BAD_REQUEST,
                "Resident model release is available only on Apple Silicon.",
            )
        components = self._parse_performance_components(
            payload, default=tuple(sorted(_PREWARM_COMPONENTS))
        )
        if self.live is not None and self.live.active_session_count:
            raise WorkerError(
                ErrorCode.WORKER_BUSY,
                "Resident models cannot be released while live captions are active.",
                retryable=True,
            )
        if self.jobs.status()["active_job_ids"] or self._prewarm_status()["running"]:
            raise WorkerError(
                ErrorCode.WORKER_BUSY,
                "Resident models cannot be released while inference is active.",
                retryable=True,
            )
        selected = {key for key in self._resident.keys() if key.split(":", 1)[0] in set(components)}
        released = self._resident.evict_idle(force=True, keys=selected)
        return {
            "released": list(released),
            "components": list(components),
            "resident_models": self._resident.status(),
        }

    @staticmethod
    def _parse_performance_components(
        payload: Mapping[str, Any], *, default: Sequence[str]
    ) -> tuple[str, ...]:
        raw = payload.get("components", list(default))
        if not isinstance(raw, list) or not raw:
            raise WorkerError(ErrorCode.BAD_REQUEST, "'components' must be a non-empty array.")
        components: list[str] = []
        for value in raw:
            component = require_string(value, "component")
            if component not in _PREWARM_COMPONENTS:
                raise WorkerError(
                    ErrorCode.BAD_REQUEST,
                    f"Unsupported performance component {component!r}.",
                )
            if component not in components:
                components.append(component)
        return tuple(components)

    def _prewarm_status(self) -> JsonObject:
        with self._prewarm_lock:
            running = self._prewarm_thread is not None and self._prewarm_thread.is_alive()
            status: JsonObject = {
                "running": running,
                "request_id": self._prewarm_request_id if running else None,
                "components": list(self._prewarm_components) if running else [],
            }
            if running and self._prewarm_skipped_components:
                status["skipped_components"] = list(self._prewarm_skipped_components)
            return status

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
                    "The live caption model is still being released.",
                    {"active_live_sessions": 0},
                    retryable=True,
                )
            self.live = None

        def run(cancel: threading.Event) -> JsonObject:
            with self._compute_lock:
                pipeline, leases = self._create_pipeline(pipeline_input)
                try:
                    return pipeline.run(pipeline_input, cancel)
                finally:
                    for lease in reversed(leases):
                        lease.close()

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

    def _create_pipeline(
        self, pipeline_input: PipelineInput
    ) -> tuple[FinalPipeline, list[BackendLease[object]]]:
        final_model = self.models.require("final_asr_en")
        asr_device = self._preferred_asr_device()
        torch_device = self._preferred_torch_device()
        diarization_model = self.models.require("diarization")
        embedding_model = self.models.require("speaker_embedding")
        alignment_model = None if self._uses_mlx_asr() else self.models.require("alignment_en")
        leases: list[BackendLease[object]] = []

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
                    "code": "ACCELERATOR_CPU_FALLBACK",
                    "source_error_code": error.code.value,
                    "retryable": True,
                },
            )

        try:
            transcriber = RetryingTranscriber(
                self._final_asr_attempts(final_model, asr_device, leases)
            )
            if alignment_model is None:
                # MLX Whisper already emits cross-attention word timestamps, so a
                # second WhisperX pass would add latency and another model load.
                aligner: Aligner = NativeWordTimestampAligner()
            else:
                aligner = RetryingAligner(
                    self._alignment_attempts(alignment_model, torch_device, leases),
                    on_fallback=report_fallback,
                )
            diarizer = RetryingDiarizer(
                self._diarization_attempts(diarization_model, torch_device, leases),
                on_fallback=report_fallback,
            )
            embedder = RetryingEmbedder(
                self._embedding_attempts(embedding_model, torch_device, leases),
                on_fallback=report_fallback,
            )
            return (
                FinalPipeline(
                    normalizer=FfmpegNormalizer(self.ffmpeg_path),
                    transcriber=transcriber,
                    aligner=aligner,
                    diarizer=diarizer,
                    embedder=embedder,
                    emit=self.emitter.emit,
                    release_backends_after_stage=self._resident_policy.stage_bounded_release,
                ),
                leases,
            )
        except Exception:
            for lease in reversed(leases):
                lease.close()
            raise

    def _final_asr_attempts(
        self,
        model_path: Path,
        asr_device: str,
        leases: list[BackendLease[object]],
    ) -> list[Transcriber]:
        if self._uses_mlx_asr():
            return [
                cast(
                    Transcriber,
                    self._cached_backend(
                        "final_asr:mlx",
                        lambda: MlxWhisperBackend(
                            model_path,
                            beam_size=5,
                            on_model_load=self._model_load_callback("mlx", "mps"),
                        ),
                        leases,
                    ),
                )
            ]
        if asr_device != "cuda":
            return [
                cast(
                    Transcriber,
                    self._cached_backend(
                        "final_asr:cpu-int8",
                        lambda: FasterWhisperBackend(
                            model_path,
                            device="cpu",
                            compute_type="int8",
                            beam_size=5,
                            on_model_load=self._model_load_callback("ctranslate2", "cpu"),
                        ),
                        leases,
                    ),
                )
            ]
        variants = (
            ("cuda-fp16-b8", "cuda", "float16", 8),
            ("cuda-fp16-b2", "cuda", "float16", 2),
            ("cuda-int8-fp16-b2", "cuda", "int8_float16", 2),
            ("cpu-int8", "cpu", "int8", 1),
        )
        return [
            cast(
                Transcriber,
                self._cached_backend(
                    f"final_asr:{variant}",
                    partial(
                        FasterWhisperBackend,
                        model_path,
                        device=device,
                        compute_type=compute,
                        beam_size=5,
                        batch_size=batch,
                        on_model_load=self._model_load_callback("ctranslate2", device),
                    ),
                    leases,
                ),
            )
            for variant, device, compute, batch in variants
        ]

    def _alignment_attempts(
        self,
        model_path: Path,
        torch_device: str,
        leases: list[BackendLease[object]],
    ) -> list[Aligner]:
        devices = ("cuda", "cpu") if torch_device == "cuda" else ("cpu",)
        return [
            cast(
                Aligner,
                self._cached_backend(
                    f"alignment:{device}",
                    partial(
                        WhisperXAligner,
                        model_path,
                        device=device,
                        on_model_load=self._model_load_callback("whisperx", device),
                    ),
                    leases,
                ),
            )
            for device in devices
        ]

    def _diarization_attempts(
        self,
        model_path: Path,
        torch_device: str,
        leases: list[BackendLease[object]],
    ) -> list[Diarizer]:
        devices = (torch_device, "cpu") if torch_device != "cpu" else ("cpu",)
        return [
            cast(
                Diarizer,
                self._cached_backend(
                    f"diarization:{device}",
                    partial(
                        PyannoteDiarizer,
                        model_path,
                        device=device,
                        on_model_load=self._model_load_callback("pyannote", device),
                    ),
                    leases,
                ),
            )
            for device in devices
        ]

    def _embedding_attempts(
        self,
        model_path: Path,
        torch_device: str,
        leases: list[BackendLease[object]],
    ) -> list[Embedder]:
        devices = (torch_device, "cpu") if torch_device != "cpu" else ("cpu",)
        return [
            cast(
                Embedder,
                self._cached_backend(
                    f"speaker_embedding:{device}",
                    partial(
                        WeSpeakerEmbedder,
                        model_path,
                        device=device,
                        on_model_load=self._model_load_callback("pyannote", device),
                    ),
                    leases,
                ),
            )
            for device in devices
        ]

    def _cached_backend(
        self,
        key: str,
        factory: Callable[[], object],
        leases: list[BackendLease[object]],
    ) -> object:
        if not self._resident_models_enabled:
            return factory()
        lease = self._resident.acquire(key, factory)
        leases.append(lease)
        return lease.value

    def _model_load_callback(self, backend: str, device: str) -> ModelLoadCallback:
        def report(component: str, duration_ms: int) -> None:
            self.emitter.emit(
                "performance_timing",
                {
                    "scope": "model_load",
                    "component": component,
                    "backend": backend,
                    "device": device,
                    "duration_ms": duration_ms,
                    "resident_cache": True,
                },
            )

        return report

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

            if torch.cuda.is_available():
                return "cuda"
            if hasattr(torch.backends, "mps") and torch.backends.mps.is_available():
                return "mps"
            return "cpu"
        except (ImportError, RuntimeError):
            return "cpu"

    @staticmethod
    def _uses_mlx_asr() -> bool:
        return platform.system() == "Darwin" and platform.machine() == "arm64"

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
