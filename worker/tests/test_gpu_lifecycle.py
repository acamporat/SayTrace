from __future__ import annotations

import threading
from pathlib import Path

from local_transcript_worker.backends import (
    RetryingAligner,
    RetryingDiarizer,
    RetryingEmbedder,
)
from local_transcript_worker.errors import ErrorCode, WorkerError
from local_transcript_worker.profiles import SpeakerEmbeddingBatch
from local_transcript_worker.schema import DiarizationSegment, WordTiming


class OomBackend:
    def __init__(self) -> None:
        self.release_calls = 0

    def release(self) -> None:
        self.release_calls += 1


class OomAligner(OomBackend):
    def align(
        self,
        audio_path: Path,
        source_id: str,
        words: list[WordTiming],
        cancel: threading.Event,
    ) -> list[WordTiming]:
        raise WorkerError(ErrorCode.GPU_OUT_OF_MEMORY, "alignment oom", retryable=True)


class CpuAligner:
    def __init__(self) -> None:
        self.calls = 0

    def align(
        self,
        audio_path: Path,
        source_id: str,
        words: list[WordTiming],
        cancel: threading.Event,
    ) -> list[WordTiming]:
        self.calls += 1
        return words


class OomDiarizer(OomBackend):
    def diarize(self, audio_path: Path, cancel: threading.Event) -> list[DiarizationSegment]:
        raise WorkerError(ErrorCode.GPU_OUT_OF_MEMORY, "diarization oom", retryable=True)


class CpuDiarizer:
    def __init__(self) -> None:
        self.calls = 0

    def diarize(self, audio_path: Path, cancel: threading.Event) -> list[DiarizationSegment]:
        self.calls += 1
        return [DiarizationSegment(0, 1_000, "speaker-0")]


class OomEmbedder(OomBackend):
    def embed_intervals(
        self,
        audio_path: Path,
        intervals_ms: list[tuple[int, int]],
        cancel: threading.Event,
    ) -> SpeakerEmbeddingBatch:
        raise WorkerError(ErrorCode.GPU_OUT_OF_MEMORY, "embedding oom", retryable=True)


class CpuEmbedder:
    def __init__(self) -> None:
        self.calls = 0

    def embed_intervals(
        self,
        audio_path: Path,
        intervals_ms: list[tuple[int, int]],
        cancel: threading.Event,
    ) -> SpeakerEmbeddingBatch:
        self.calls += 1
        return SpeakerEmbeddingBatch(((1.0, 0.0),), 1_000)


def test_alignment_oom_selects_cpu_fallback_and_reports_stable_stage() -> None:
    gpu = OomAligner()
    cpu = CpuAligner()
    fallback: list[tuple[str, str]] = []
    backend = RetryingAligner(
        [gpu, cpu],
        on_fallback=lambda stage, error: fallback.append((stage, error.code.value)),
    )

    result = backend.align(Path("unused"), "asset", [], threading.Event())

    assert result == []
    assert gpu.release_calls == 1
    assert cpu.calls == 1
    assert fallback == [("align", "GPU_OUT_OF_MEMORY")]


def test_diarization_oom_selects_cpu_fallback_and_reports_stable_stage() -> None:
    gpu = OomDiarizer()
    cpu = CpuDiarizer()
    fallback: list[tuple[str, str]] = []
    backend = RetryingDiarizer(
        [gpu, cpu],
        on_fallback=lambda stage, error: fallback.append((stage, error.code.value)),
    )

    result = backend.diarize(Path("unused"), threading.Event())

    assert result == [DiarizationSegment(0, 1_000, "speaker-0")]
    assert gpu.release_calls == 1
    assert cpu.calls == 1
    assert fallback == [("diarize", "GPU_OUT_OF_MEMORY")]


def test_embedding_oom_selects_cpu_fallback_and_reports_stable_stage() -> None:
    gpu = OomEmbedder()
    cpu = CpuEmbedder()
    fallback: list[tuple[str, str]] = []
    backend = RetryingEmbedder(
        [gpu, cpu],
        on_fallback=lambda stage, error: fallback.append((stage, error.code.value)),
    )

    result = backend.embed_intervals(Path("unused"), [(0, 1_000)], threading.Event())

    assert result == SpeakerEmbeddingBatch(((1.0, 0.0),), 1_000)
    assert gpu.release_calls == 1
    assert cpu.calls == 1
    assert fallback == [("identify", "GPU_OUT_OF_MEMORY")]
