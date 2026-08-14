from __future__ import annotations

import io
import json
from pathlib import Path

import pytest

from local_transcript_worker.app import WorkerApp
from local_transcript_worker.errors import ErrorCode, WorkerError
from local_transcript_worker.pipeline import PipelineInput
from local_transcript_worker.protocol import FrameWriter
from local_transcript_worker.schema import PipelineCheckpoint, Request


class FakeWarmBackend:
    def __init__(self) -> None:
        self.prewarm_calls = 0
        self.release_calls = 0

    def prewarm(self) -> None:
        self.prewarm_calls += 1

    def release(self) -> None:
        self.release_calls += 1


def _app(
    tmp_path: Path,
    *,
    resident_models_enabled: bool | None = True,
    model_cache_idle_seconds: float | None = 60,
    model_cache_max_entries: int | None = None,
    physical_memory_bytes_override: int | None = None,
) -> WorkerApp:
    ffmpeg = tmp_path / "ffmpeg"
    ffmpeg.write_bytes(b"test")
    library = tmp_path / "library"
    library.mkdir()
    app = WorkerApp(
        FrameWriter(io.BytesIO()),
        model_root=tmp_path / "models",
        approved_roots=[library],
        ffmpeg_path=ffmpeg,
        setup_enabled=True,
        heartbeat_seconds=3600,
        model_cache_idle_seconds=model_cache_idle_seconds,
        model_cache_max_entries=model_cache_max_entries,
        physical_memory_bytes_override=physical_memory_bytes_override,
        resident_models_enabled_override=resident_models_enabled,
    )
    return app


def test_prewarm_protocol_is_async_selected_and_privacy_safe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    app = _app(tmp_path)
    backend = FakeWarmBackend()
    events: list[tuple[str, dict[str, object]]] = []
    monkeypatch.setattr(app.emitter, "emit", lambda event, payload: events.append((event, payload)))
    monkeypatch.setattr(app, "_prewarm_backend", lambda _component, _leases: backend)
    try:
        result = app.handle_request(
            Request(
                request_id="warm-1",
                command="performance.prewarm",
                payload={"components": ["final_asr"]},
            )
        )
        with app._prewarm_lock:
            thread = app._prewarm_thread
        assert thread is not None
        thread.join(timeout=2)

        assert result == {
            "accepted": True,
            "state": "warming",
            "request_id": "warm-1",
            "components": ["final_asr"],
        }
        assert backend.prewarm_calls == 1
        assert [event for event, _payload in events] == [
            "performance_prewarm_started",
            "performance_timing",
            "performance_prewarm_complete",
        ]
        serialized = json.dumps(events).casefold()
        assert "audio_path" not in serialized
        assert "transcript" not in serialized
    finally:
        app.close()


def test_health_exposes_bounded_low_memory_policy_without_paths(tmp_path: Path) -> None:
    app = _app(
        tmp_path,
        model_cache_idle_seconds=None,
        physical_memory_bytes_override=8 * 1024**3,
    )
    try:
        health = app.handle_request(
            Request(request_id="health-policy", command="health", payload={})
        )

        policy = health["performance"]["resident_model_policy"]
        assert policy == {
            "enabled": True,
            "memory_tier": "low",
            "physical_memory_mib": 8192,
            "idle_timeout_ms": 120_000,
            "max_entries": 2,
            "stage_bounded_release": True,
            "prewarm_components": ["final_asr"],
            "adaptive": True,
            "idle_overridden": False,
            "max_entries_overridden": False,
        }
        serialized = json.dumps(policy).casefold()
        assert "path" not in serialized
        assert "transcript" not in serialized
    finally:
        app.close()


def test_low_memory_prewarm_runs_only_final_asr_and_reports_skipped_components(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    app = _app(
        tmp_path,
        model_cache_idle_seconds=None,
        physical_memory_bytes_override=8 * 1024**3,
    )
    backend = FakeWarmBackend()
    events: list[tuple[str, dict[str, object]]] = []
    monkeypatch.setattr(app.emitter, "emit", lambda event, payload: events.append((event, payload)))
    monkeypatch.setattr(app, "_prewarm_backend", lambda _component, _leases: backend)
    try:
        result = app.handle_request(
            Request(
                request_id="warm-low",
                command="performance.prewarm",
                payload={
                    "components": [
                        "final_asr",
                        "diarization",
                        "speaker_embedding",
                    ]
                },
            )
        )
        with app._prewarm_lock:
            thread = app._prewarm_thread
        assert thread is not None
        thread.join(timeout=2)

        assert result == {
            "accepted": True,
            "state": "warming",
            "request_id": "warm-low",
            "components": ["final_asr"],
            "skipped_components": ["diarization", "speaker_embedding"],
        }
        assert backend.prewarm_calls == 1
        started = next(payload for event, payload in events if event.endswith("_started"))
        complete = next(payload for event, payload in events if event.endswith("_complete"))
        assert started["components"] == ["final_asr"]
        assert started["skipped_components"] == ["diarization", "speaker_embedding"]
        assert complete["components"] == ["final_asr"]
        assert complete["skipped_components"] == ["diarization", "speaker_embedding"]
    finally:
        app.close()


def test_low_memory_prewarm_can_truthfully_skip_an_unsupported_working_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    app = _app(
        tmp_path,
        model_cache_idle_seconds=None,
        physical_memory_bytes_override=8 * 1024**3,
    )
    events: list[tuple[str, dict[str, object]]] = []
    monkeypatch.setattr(app.emitter, "emit", lambda event, payload: events.append((event, payload)))
    monkeypatch.setattr(
        app,
        "_prewarm_backend",
        lambda _component, _leases: pytest.fail("a skipped component must not load"),
    )
    try:
        result = app.handle_request(
            Request(
                request_id="warm-low-skip",
                command="performance.prewarm",
                payload={"components": ["diarization"]},
            )
        )

        assert result == {
            "accepted": False,
            "state": "skipped_by_memory_policy",
            "request_id": "warm-low-skip",
            "components": [],
            "skipped_components": ["diarization"],
        }
        assert events == [
            (
                "performance_prewarm_complete",
                {
                    "request_id": "warm-low-skip",
                    "components": [],
                    "skipped_components": ["diarization"],
                },
            )
        ]
        assert app._prewarm_thread is None
    finally:
        app.close()


def test_release_protocol_evicts_selected_idle_models(tmp_path: Path) -> None:
    app = _app(tmp_path)
    backend = FakeWarmBackend()
    lease = app._resident.acquire("final_asr:mlx", lambda: backend)
    lease.close()
    try:
        result = app.handle_request(
            Request(
                request_id="release-1",
                command="performance.release",
                payload={"components": ["final_asr"]},
            )
        )

        assert result["released"] == ["final_asr:mlx"]
        assert backend.release_calls == 1
        assert app._resident.keys() == ()
    finally:
        app.close()


def test_performance_protocol_rejects_unknown_components(tmp_path: Path) -> None:
    app = _app(tmp_path)
    try:
        with pytest.raises(WorkerError) as raised:
            app.handle_request(
                Request(
                    request_id="warm-bad",
                    command="performance.prewarm",
                    payload={"components": ["unknown"]},
                )
            )

        assert raised.value.code is ErrorCode.BAD_REQUEST
    finally:
        app.close()


@pytest.mark.parametrize("busy_owner", ["job", "prewarm"])
def test_live_start_is_disposable_while_final_inference_owns_the_accelerator(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    busy_owner: str,
) -> None:
    app = _app(tmp_path)
    if busy_owner == "job":
        monkeypatch.setattr(
            app.jobs,
            "status",
            lambda: {"active_job_ids": ["final-job"], "queued_jobs": 0},
        )
    else:
        monkeypatch.setattr(
            app,
            "_prewarm_status",
            lambda: {
                "running": True,
                "request_id": "warm-1",
                "components": ["final_asr"],
            },
        )
    monkeypatch.setattr(
        app.models,
        "require",
        lambda _key: pytest.fail("busy live.start must not load a model"),
    )
    try:
        with pytest.raises(WorkerError) as raised:
            app.handle_request(
                Request(
                    request_id=f"live-{busy_owner}",
                    command="live.start",
                    payload={
                        "session_id": "session-1",
                        "streams": {"microphone": "microphone"},
                    },
                )
            )

        assert raised.value.code is ErrorCode.WORKER_BUSY
        assert raised.value.retryable is True
        assert app.live is None
    finally:
        app.close()


def test_non_apple_platform_uses_nonresident_stage_release_policy(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(WorkerApp, "_uses_mlx_asr", staticmethod(lambda: False))
    app = _app(tmp_path, resident_models_enabled=None)
    monkeypatch.setattr(app.models, "require", lambda key: Path(key))
    monkeypatch.setattr(app, "_preferred_asr_device", lambda: "cpu")
    monkeypatch.setattr(app, "_preferred_torch_device", lambda: "cpu")
    request = PipelineInput(
        job_id="windows-policy",
        pipeline_version=app.manifest.pipeline_version,
        sources=(),
        workspace=tmp_path,
        diarization_asset_id="unused",
        profiles=(),
        match_policy=None,
        checkpoint=PipelineCheckpoint(app.manifest.pipeline_version),
    )
    try:
        pipeline, leases = app._create_pipeline(request)

        assert app._resident_models_enabled is False
        assert pipeline.release_backends_after_stage is True
        assert leases == []
        assert app._resident.keys() == ()
        with pytest.raises(WorkerError) as raised:
            app.handle_request(
                Request(
                    request_id="warm-windows",
                    command="performance.prewarm",
                    payload={"components": ["final_asr"]},
                )
            )
        assert raised.value.code is ErrorCode.BAD_REQUEST
    finally:
        app.close()


@pytest.mark.parametrize(
    ("memory_gib", "release_after_stage"),
    [(8, True), (16, True), (24, False)],
)
def test_apple_pipeline_applies_memory_tier_stage_release_policy(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    memory_gib: int,
    release_after_stage: bool,
) -> None:
    app = _app(
        tmp_path,
        model_cache_idle_seconds=None,
        physical_memory_bytes_override=memory_gib * 1024**3,
    )
    monkeypatch.setattr(app.models, "require", lambda key: Path(key))
    monkeypatch.setattr(app, "_uses_mlx_asr", lambda: True)
    monkeypatch.setattr(app, "_preferred_asr_device", lambda: "mps")
    monkeypatch.setattr(app, "_preferred_torch_device", lambda: "mps")
    monkeypatch.setattr(app, "_final_asr_attempts", lambda *_args: [object()])
    monkeypatch.setattr(app, "_diarization_attempts", lambda *_args: [object()])
    monkeypatch.setattr(app, "_embedding_attempts", lambda *_args: [object()])
    request = PipelineInput(
        job_id=f"apple-{memory_gib}-gib-policy",
        pipeline_version=app.manifest.pipeline_version,
        sources=(),
        workspace=tmp_path,
        diarization_asset_id="unused",
        profiles=(),
        match_policy=None,
        checkpoint=PipelineCheckpoint(app.manifest.pipeline_version),
    )
    try:
        pipeline, leases = app._create_pipeline(request)

        assert pipeline.release_backends_after_stage is release_after_stage
        assert leases == []
    finally:
        app.close()
