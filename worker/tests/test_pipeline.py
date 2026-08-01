from __future__ import annotations

import base64
import json
import struct
import threading
import wave
from pathlib import Path

import pytest

import local_transcript_worker.pipeline as pipeline_module
from local_transcript_worker.errors import ErrorCode, WorkerError
from local_transcript_worker.pipeline import (
    ArtifactStore,
    FinalPipeline,
    PipelineInput,
    _plan_asr_chunks,
    _select_playback_artifact,
)
from local_transcript_worker.profiles import (
    MatchPolicy,
    SpeakerEmbeddingBatch,
    VoiceProfile,
)
from local_transcript_worker.schema import (
    DiarizationSegment,
    PipelineCheckpoint,
    SourceAsset,
    WordTiming,
)


class FakeNormalizer:
    def __init__(self) -> None:
        self.calls = 0

    def normalize(self, source: Path, target: Path, cancel: threading.Event) -> Path:
        assert not cancel.is_set()
        self.calls += 1
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(source.read_bytes())
        return target


class FakeTranscriber:
    def __init__(self) -> None:
        self.calls = 0

    def transcribe(
        self, audio_path: Path, source_id: str, cancel: threading.Event
    ) -> list[WordTiming]:
        assert audio_path.exists()
        assert not cancel.is_set()
        self.calls += 1
        return [
            WordTiming(
                word_id=f"{source_id}:0",
                text="hello",
                model_text="hello",
                start_ms=100,
                end_ms=500,
                source_id=source_id,
                confidence=0.95,
            )
        ]


class FakeAligner:
    def align(
        self,
        audio_path: Path,
        source_id: str,
        words: list[WordTiming],
        cancel: threading.Event,
    ) -> list[WordTiming]:
        assert audio_path.exists() and not cancel.is_set()
        return words


class FakeDiarizer:
    def diarize(self, audio_path: Path, cancel: threading.Event) -> list[DiarizationSegment]:
        assert audio_path.exists() and not cancel.is_set()
        return [DiarizationSegment(0, 1000, "speaker-0")]


class FakeEmbedder:
    def embed_intervals(
        self,
        audio_path: Path,
        intervals_ms: list[tuple[int, int]],
        cancel: threading.Event,
    ) -> SpeakerEmbeddingBatch:
        assert audio_path.exists() and intervals_ms and not cancel.is_set()
        return SpeakerEmbeddingBatch(((1.0, 0.0),), 1_000)


class FakeEmptyEmbedder:
    def embed_intervals(
        self,
        audio_path: Path,
        intervals_ms: list[tuple[int, int]],
        cancel: threading.Event,
    ) -> SpeakerEmbeddingBatch:
        assert audio_path.exists() and intervals_ms and not cancel.is_set()
        return SpeakerEmbeddingBatch((), 0)


def test_pipeline_runs_with_fakes_and_resumes_batches(tmp_path: Path) -> None:
    source_path = tmp_path / "source.wav"
    source_path.write_bytes(b"fake-wave")
    workspace = tmp_path / "job"
    workspace.mkdir()
    normalizer = FakeNormalizer()
    transcriber = FakeTranscriber()
    events: list[tuple[str, dict[str, object]]] = []
    pipeline = FinalPipeline(
        normalizer=normalizer,
        transcriber=transcriber,
        aligner=FakeAligner(),
        diarizer=FakeDiarizer(),
        embedder=FakeEmbedder(),
        emit=lambda event, payload: events.append((event, payload)),
        correlation_factory=lambda paths: lambda left, right: None,
    )
    profile = VoiceProfile.from_dict(
        {
            "profile_id": "alex",
            "name": "Alex",
            "embeddings": [[1.0, 0.0]] * 3,
            "sample_durations_ms": [10_000] * 3,
            "explicitly_confirmed": True,
        }
    )
    checkpoint = PipelineCheckpoint("pipeline-1")
    request = PipelineInput(
        job_id="job-1",
        pipeline_version="pipeline-1",
        sources=(SourceAsset("asset-1", source_path, "import", priority=20),),
        workspace=workspace,
        diarization_asset_id="asset-1",
        profiles=(profile,),
        match_policy=MatchPolicy("calibration-1", 0.9, 0.08, 0.7),
        checkpoint=checkpoint,
    )

    first = pipeline.run(request, threading.Event())

    assert first["turn_count"] == 1
    playback = Path(str(first["playback_artifact_path"])).resolve(strict=True)
    assert playback == (workspace / "normalized" / "asset-1.wav").resolve(strict=True)
    assert workspace.resolve(strict=True) in playback.parents
    candidates = first["speaker_candidates"]
    assert isinstance(candidates, list)
    assert len(candidates) == 1
    candidate = candidates[0]
    assert candidate["cluster_label"] == "speaker-0"
    assert candidate["clean_duration_ms"] == 1_000
    encoded_embedding = str(candidate["embedding_base64"])
    decoded_embedding = struct.unpack("<2f", base64.b64decode(encoded_embedding))
    assert decoded_embedding == pytest.approx((1.0, 0.0))
    assert normalizer.calls == 1
    assert transcriber.calls == 1
    canonical = Path(str(first["canonical_artifact_path"]))
    data = json.loads(canonical.read_text(encoding="utf-8"))
    assert data["turns"][0]["speaker_name"] == "Alex"
    assert data["turns"][0]["speaker_state"] == "Matched"
    assert data["speaker_matches"] == {
        "speaker-0": {
            "profile_id": "alex",
            "name": "Alex",
            "state": "Matched",
            "similarity": 1.0,
            "margin": 2.0,
            "calibration_id": "calibration-1",
        }
    }
    assert "embedding" not in json.dumps(data).casefold()
    index_path = Path(str(checkpoint.stage_results["index:canonical"]))
    index_data = json.loads(index_path.read_text(encoding="utf-8"))
    assert index_data["speaker_matches"] == data["speaker_matches"]
    assert any(event == "pipeline_batch" for event, _payload in events)
    for artifact_path in checkpoint.stage_results.values():
        artifact_text = Path(artifact_path).read_text(encoding="utf-8")
        assert "embedding" not in artifact_text.casefold()
        assert encoded_embedding not in artifact_text
    event_text = json.dumps(events)
    assert "embedding_base64" not in event_text
    assert encoded_embedding not in event_text

    second = pipeline.run(request, threading.Event())

    assert second["turn_count"] == 1
    assert second["speaker_candidates"] == first["speaker_candidates"]
    assert second["playback_artifact_path"] == first["playback_artifact_path"]
    assert normalizer.calls == 1
    assert transcriber.calls == 1


def test_pipeline_omits_candidate_when_embedder_has_no_clean_speech(
    tmp_path: Path,
) -> None:
    source_path = tmp_path / "source.wav"
    source_path.write_bytes(b"fake-wave")
    workspace = tmp_path / "job"
    workspace.mkdir()
    pipeline = FinalPipeline(
        normalizer=FakeNormalizer(),
        transcriber=FakeTranscriber(),
        aligner=FakeAligner(),
        diarizer=FakeDiarizer(),
        embedder=FakeEmptyEmbedder(),
        emit=lambda _event, _payload: None,
        correlation_factory=lambda paths: lambda left, right: None,
    )
    request = PipelineInput(
        job_id="job-no-clean-speech",
        pipeline_version="pipeline-1",
        sources=(SourceAsset("asset-1", source_path, "import", priority=20),),
        workspace=workspace,
        diarization_asset_id="asset-1",
        profiles=(),
        match_policy=None,
        checkpoint=PipelineCheckpoint("pipeline-1"),
    )

    result = pipeline.run(request, threading.Event())

    assert result["speaker_candidates"] == []
    artifact_text = Path(str(result["canonical_artifact_path"])).read_text(encoding="utf-8")
    assert "embedding" not in artifact_text.casefold()


def test_asr_chunks_resume_at_ten_minute_boundaries_with_overlap(tmp_path: Path) -> None:
    source = tmp_path / "long.wav"
    with wave.open(str(source), "wb") as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(1)
        output.writeframes(b"\0\0" * 1200)
    workspace = tmp_path / "workspace"
    workspace.mkdir()

    chunks = _plan_asr_chunks("asset", source, workspace)

    assert len(chunks) == 2
    assert chunks[0].core_start_ms == 0
    assert chunks[0].core_end_ms == 600_000
    assert chunks[1].audio_start_ms == 595_000
    assert chunks[1].core_start_ms == 600_000
    assert chunks[1].final_chunk is True


def test_mixed_normalized_source_is_preferred_for_playback_artifact(tmp_path: Path) -> None:
    workspace = tmp_path / "job"
    normalized_root = workspace / "normalized"
    normalized_root.mkdir(parents=True)
    imported = normalized_root / "import.wav"
    mixed = normalized_root / "mixed.wav"
    imported.write_bytes(b"import")
    mixed.write_bytes(b"mixed")
    request = PipelineInput(
        job_id="job-mixed-playback",
        pipeline_version="pipeline-1",
        sources=(
            SourceAsset("import", tmp_path / "source.mkv", "import", priority=20),
            SourceAsset("mixed", tmp_path / "mixed.flac", "mixed", priority=0),
            SourceAsset("mic", tmp_path / "mic.flac", "microphone", priority=10),
        ),
        workspace=workspace,
        diarization_asset_id="mixed",
        profiles=(),
        match_policy=None,
        checkpoint=PipelineCheckpoint("pipeline-1"),
    )

    selected = _select_playback_artifact(
        request,
        {
            "import": imported,
            "mixed": mixed,
        },
    )

    assert selected == mixed.resolve(strict=True)


def test_artifact_is_fsynced_before_atomic_publish(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: list[int] = []
    monkeypatch.setattr(pipeline_module.os, "fsync", lambda descriptor: calls.append(descriptor))
    store = ArtifactStore(tmp_path)

    target = store.write("transcribe", "batch-1", {"ready": True})

    assert target.is_file()
    assert calls
    assert not list(target.parent.glob("*.partial"))


def test_oom_retry_releases_large_batch_before_smaller_fallback() -> None:
    from local_transcript_worker.backends import RetryingTranscriber

    class OomBackend:
        released = False

        def transcribe(self, audio_path, source_id, cancel):
            raise WorkerError(ErrorCode.GPU_OUT_OF_MEMORY, "oom", retryable=True)

        def release(self) -> None:
            self.released = True

    class SuccessBackend:
        def transcribe(self, audio_path, source_id, cancel):
            return []

    first = OomBackend()
    retrying = RetryingTranscriber([first, SuccessBackend()])

    assert retrying.transcribe(Path("unused"), "asset", threading.Event()) == []
    assert first.released is True


def test_pipeline_releases_each_heavy_stage_before_loading_the_next(tmp_path: Path) -> None:
    active: set[str] = set()
    lifecycle: list[str] = []

    def enter(stage: str) -> None:
        assert not active
        active.add(stage)
        lifecycle.append(f"load:{stage}")

    def leave(stage: str) -> None:
        active.discard(stage)
        lifecycle.append(f"release:{stage}")

    class TrackingTranscriber(FakeTranscriber):
        def transcribe(self, audio_path, source_id, cancel):
            enter("transcribe")
            return super().transcribe(audio_path, source_id, cancel)

        def release(self) -> None:
            leave("transcribe")

    class TrackingAligner(FakeAligner):
        def align(self, audio_path, source_id, words, cancel):
            enter("align")
            return super().align(audio_path, source_id, words, cancel)

        def release(self) -> None:
            leave("align")

    class TrackingDiarizer(FakeDiarizer):
        def diarize(self, audio_path, cancel):
            enter("diarize")
            return super().diarize(audio_path, cancel)

        def release(self) -> None:
            leave("diarize")

    class TrackingEmbedder(FakeEmbedder):
        def embed_intervals(self, audio_path, intervals_ms, cancel):
            enter("identify")
            return super().embed_intervals(audio_path, intervals_ms, cancel)

        def release(self) -> None:
            leave("identify")

    source_path = tmp_path / "source.wav"
    source_path.write_bytes(b"fake-wave")
    workspace = tmp_path / "job"
    workspace.mkdir()
    pipeline = FinalPipeline(
        normalizer=FakeNormalizer(),
        transcriber=TrackingTranscriber(),
        aligner=TrackingAligner(),
        diarizer=TrackingDiarizer(),
        embedder=TrackingEmbedder(),
        emit=lambda _event, _payload: None,
        correlation_factory=lambda paths: lambda left, right: None,
    )
    request = PipelineInput(
        job_id="job-lifecycle",
        pipeline_version="pipeline-1",
        sources=(SourceAsset("asset-1", source_path, "import", priority=20),),
        workspace=workspace,
        diarization_asset_id="asset-1",
        profiles=(),
        match_policy=None,
        checkpoint=PipelineCheckpoint("pipeline-1"),
    )

    pipeline.run(request, threading.Event())

    assert active == set()
    assert lifecycle == [
        "load:transcribe",
        "release:transcribe",
        "load:align",
        "release:align",
        "load:diarize",
        "release:diarize",
        "load:identify",
        "release:identify",
    ]
