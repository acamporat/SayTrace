from __future__ import annotations

import sys
import threading
import wave
from pathlib import Path
from types import ModuleType, SimpleNamespace

import pytest

import local_transcript_worker.backends as backends
from local_transcript_worker.backends import (
    MlxWhisperBackend,
    _clamp_intervals_to_audio,
    _load_offline_sentence_tokenizer,
    _mlx_result_words,
    _read_mlx_waveform,
    _translate_ml_error,
)
from local_transcript_worker.errors import ErrorCode
from local_transcript_worker.resident import ResidentBackendCache


class SentenceTokenizer:
    def span_tokenize(self, text: str) -> list[tuple[int, int]]:
        return [(0, len(text))]


def test_missing_punkt_data_uses_an_offline_sentence_tokenizer() -> None:
    def missing(_resource: str) -> object:
        raise LookupError("punkt_tab is not installed")

    tokenizer = _load_offline_sentence_tokenizer(
        "tokenizers/punkt_tab/english.pickle", missing, SentenceTokenizer
    )

    assert tokenizer.span_tokenize("No download is needed.") == [(0, 22)]


def test_speaker_intervals_are_clamped_to_the_real_waveform_extent() -> None:
    intervals = _clamp_intervals_to_audio(
        [(-100, 900), (2_403_000, 2_405_000), (2_404_000, 2_406_000)],
        sample_count=2_403_904 * 16,
        sample_rate=16_000,
    )

    assert intervals == [(0, 900), (2_403_000, 2_403_903)]


def test_speaker_intervals_shorter_than_embedding_minimum_are_removed() -> None:
    intervals = _clamp_intervals_to_audio(
        [(100, 849), (100, 850)], sample_count=16_000, sample_rate=16_000
    )

    assert intervals == [(100, 850)]


def test_mlx_result_maps_native_word_timestamps() -> None:
    words = _mlx_result_words(
        {
            "segments": [
                {"words": [{"word": " hello", "start": 0.25, "end": 0.7, "probability": 0.91}]}
            ]
        },
        "asset",
        threading.Event(),
    )

    assert len(words) == 1
    assert words[0].text == "hello"
    assert words[0].start_ms == 250
    assert words[0].end_ms == 700
    assert words[0].confidence == 0.91


def test_mlx_backend_uses_supported_best_of_decoder(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, object] = {}
    load_calls: list[tuple[str, object]] = []
    load_timings: list[tuple[str, int]] = []

    class FakeHolder:
        model: object | None = None
        model_path: str | None = None

        @classmethod
        def get_model(cls, model_path: str, dtype: object) -> object:
            cls.model = object()
            cls.model_path = model_path
            load_calls.append((model_path, dtype))
            return cls.model

    def transcribe(_audio: object, **options: object) -> dict[str, object]:
        captured.update(options)
        return {"segments": [{"words": [{"word": " hello", "start": 0.0, "end": 0.25}]}]}

    monkeypatch.setattr(backends, "_read_mlx_waveform", lambda _path: [0.0])
    mlx_package = ModuleType("mlx")
    mlx_core = ModuleType("mlx.core")
    mlx_core.float16 = object()  # type: ignore[attr-defined]
    mlx_package.core = mlx_core  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "mlx", mlx_package)
    monkeypatch.setitem(sys.modules, "mlx.core", mlx_core)
    monkeypatch.setitem(
        sys.modules,
        "mlx_whisper",
        SimpleNamespace(transcribe=transcribe),
    )
    original_import_module = backends.importlib.import_module
    monkeypatch.setattr(
        backends.importlib,
        "import_module",
        lambda name: (
            SimpleNamespace(ModelHolder=FakeHolder)
            if name == "mlx_whisper.transcribe"
            else original_import_module(name)
        ),
    )

    backend = MlxWhisperBackend(
        Path("model"),
        beam_size=5,
        on_model_load=lambda component, duration: load_timings.append((component, duration)),
    )
    backend.prewarm()
    words = backend.transcribe(Path("audio.wav"), "asset", threading.Event())

    assert words[0].text == "hello"
    assert captured["best_of"] == 5
    assert "beam_size" not in captured
    assert len(load_calls) == 1
    assert load_timings and load_timings[0][0] == "final_asr"


def test_mlx_audio_loader_downmixes_and_resamples_pcm(tmp_path: Path) -> None:
    pytest.importorskip("numpy")
    pytest.importorskip("scipy")
    source = tmp_path / "stereo.wav"
    with wave.open(str(source), "wb") as output:
        output.setnchannels(2)
        output.setsampwidth(2)
        output.setframerate(48_000)
        output.writeframes((1000).to_bytes(2, "little", signed=True) * 2 * 480)

    values = _read_mlx_waveform(source)

    assert 159 <= len(values) <= 161
    assert abs(float(values.mean()) - (1000 / 32768)) < 0.001


def test_stale_mlx_cache_eviction_does_not_clear_the_active_live_model(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    live_model = object()

    class FakeHolder:
        model: object | None = live_model
        model_path: str | None = "live-model"

    original_import_module = backends.importlib.import_module
    monkeypatch.setattr(
        backends.importlib,
        "import_module",
        lambda name: (
            SimpleNamespace(ModelHolder=FakeHolder)
            if name == "mlx_whisper.transcribe"
            else original_import_module(name)
        ),
    )
    monkeypatch.setattr(backends, "_release_cuda_cache", lambda: None)
    cache = ResidentBackendCache(idle_seconds=60, start_reaper=False)
    lease = cache.acquire("final_asr:mlx", lambda: MlxWhisperBackend(Path("final-model")))
    lease.close()

    assert cache.evict_idle(force=True) == ("final_asr:mlx",)
    assert FakeHolder.model is live_model
    assert FakeHolder.model_path == "live-model"

    MlxWhisperBackend(Path("live-model")).release()
    assert FakeHolder.model is None
    assert FakeHolder.model_path is None
    cache.close()


@pytest.mark.parametrize(
    ("message", "expected"),
    [
        ("Metal failed to allocate 8192 bytes", ErrorCode.GPU_OUT_OF_MEMORY),
        ("operator is not implemented for MPS backend", ErrorCode.ACCELERATOR_UNAVAILABLE),
    ],
)
def test_apple_accelerator_failures_are_retryable(message: str, expected: ErrorCode) -> None:
    error = _translate_ml_error(RuntimeError(message), "accelerator failed")

    assert error.code is expected
    assert error.retryable
