"""Lazy real ML/media backends.

Importing this module never imports torch, CUDA, faster-whisper, WhisperX, pyannote,
or NumPy. This keeps health checks and protocol tests useful on clean machines.
"""

from __future__ import annotations

import gc
import importlib
import math
import os
import subprocess
import tempfile
import threading
import wave
from collections.abc import Callable, Iterator, Sequence
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Protocol, cast

from .errors import ErrorCode, WorkerError, missing_dependency
from .merge import join_tokens
from .profiles import SpeakerEmbeddingBatch
from .schema import DiarizationSegment, WordTiming


class Normalizer(Protocol):
    def normalize(self, source: Path, target: Path, cancel: threading.Event) -> Path: ...


class Transcriber(Protocol):
    def transcribe(
        self, audio_path: Path, source_id: str, cancel: threading.Event
    ) -> list[WordTiming]: ...


class LiveTranscriber(Protocol):
    def transcribe_pcm(self, pcm: bytes, sample_rate: int, channels: int) -> str: ...


class Aligner(Protocol):
    def align(
        self,
        audio_path: Path,
        source_id: str,
        words: Sequence[WordTiming],
        cancel: threading.Event,
    ) -> list[WordTiming]: ...


class Diarizer(Protocol):
    def diarize(self, audio_path: Path, cancel: threading.Event) -> list[DiarizationSegment]: ...


class Embedder(Protocol):
    def embed_intervals(
        self,
        audio_path: Path,
        intervals_ms: Sequence[tuple[int, int]],
        cancel: threading.Event,
    ) -> SpeakerEmbeddingBatch: ...


FallbackCallback = Callable[[str, WorkerError], None]
_WHISPERX_SENTENCE_TOKENIZER_LOCK = threading.Lock()


def _cancelled(cancel: threading.Event) -> None:
    if cancel.is_set():
        raise WorkerError(ErrorCode.CANCELLED, "Job was cancelled.")


def _release_cuda_cache() -> None:
    gc.collect()
    try:
        import torch

        if torch.cuda.is_available():
            torch.cuda.empty_cache()
    except (ImportError, RuntimeError):
        pass


def release_backend(backend: object | None) -> None:
    """Release a lazy ML backend without allowing cleanup errors to mask job results."""

    if backend is None:
        return
    release = getattr(backend, "release", None)
    if callable(release):
        try:
            release()
        except Exception:
            # Cleanup is best effort. The process remains recoverable and the next
            # backend load still starts by asking PyTorch to empty its CUDA cache.
            _release_cuda_cache()
    else:
        _release_cuda_cache()


class FfmpegNormalizer:
    def __init__(self, ffmpeg_path: Path) -> None:
        self.ffmpeg_path = ffmpeg_path.resolve(strict=True)

    def normalize(self, source: Path, target: Path, cancel: threading.Event) -> Path:
        _cancelled(cancel)
        target.parent.mkdir(parents=True, exist_ok=True)
        partial = target.with_suffix(target.suffix + ".partial.wav")
        command = [
            str(self.ffmpeg_path),
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            str(source),
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-af",
            "loudnorm=I=-23:TP=-2:LRA=7",
            "-c:a",
            "pcm_s16le",
            str(partial),
        ]
        creation_flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            creationflags=creation_flags,
        )
        while process.poll() is None:
            if cancel.wait(0.1):
                process.terminate()
                process.wait(timeout=10)
                partial.unlink(missing_ok=True)
                _cancelled(cancel)
        stderr = process.communicate()[1].decode("utf-8", errors="replace")
        if process.returncode:
            partial.unlink(missing_ok=True)
            raise WorkerError(
                ErrorCode.UNSUPPORTED_AUDIO,
                "FFmpeg could not normalize this media file.",
                {"exit_code": process.returncode, "stderr_tail": stderr[-2_000:]},
            )
        partial.replace(target)
        return target


class FasterWhisperBackend:
    def __init__(
        self,
        model_path: Path,
        *,
        device: str,
        compute_type: str,
        beam_size: int = 5,
        vad_filter: bool = True,
        batch_size: int = 1,
    ) -> None:
        self.model_path = model_path
        self.device = device
        self.compute_type = compute_type
        self.beam_size = beam_size
        self.vad_filter = vad_filter
        self.batch_size = batch_size
        self._model: Any = None
        self._load_lock = threading.Lock()

    def _load(self) -> Any:
        if self._model is not None:
            return self._model
        with self._load_lock:
            if self._model is None:
                try:
                    from faster_whisper import BatchedInferencePipeline, WhisperModel
                except ImportError as exc:
                    raise missing_dependency("faster-whisper", "transcription") from exc
                try:
                    model = WhisperModel(
                        str(self.model_path),
                        device=self.device,
                        compute_type=self.compute_type,
                        local_files_only=True,
                    )
                    self._model = (
                        BatchedInferencePipeline(model=model) if self.batch_size > 1 else model
                    )
                except Exception as exc:
                    raise _translate_ml_error(
                        exc, "Unable to load the local Whisper model."
                    ) from exc
        return self._model

    def transcribe(
        self, audio_path: Path, source_id: str, cancel: threading.Event
    ) -> list[WordTiming]:
        _cancelled(cancel)
        try:
            options: dict[str, Any] = {
                "language": "en",
                "task": "transcribe",
                "beam_size": self.beam_size,
                "temperature": [0.0, 0.2, 0.4, 0.6, 0.8],
                "vad_filter": self.vad_filter,
                "word_timestamps": True,
                "condition_on_previous_text": True,
            }
            if self.batch_size > 1:
                options["batch_size"] = self.batch_size
            segments, _info = self._load().transcribe(str(audio_path), **options)
            result: list[WordTiming] = []
            sequence = 0
            for segment in segments:
                _cancelled(cancel)
                raw_words = list(getattr(segment, "words", []) or [])
                if raw_words:
                    for word in raw_words:
                        result.append(
                            WordTiming(
                                word_id=f"{source_id}:{sequence}",
                                text=str(word.word).strip(),
                                model_text=str(word.word).strip(),
                                start_ms=max(0, round(float(word.start) * 1000)),
                                end_ms=max(0, round(float(word.end) * 1000)),
                                source_id=source_id,
                                confidence=(
                                    float(word.probability)
                                    if getattr(word, "probability", None) is not None
                                    else None
                                ),
                            )
                        )
                        sequence += 1
                else:
                    tokens = str(segment.text).strip().split()
                    start_ms = max(0, round(float(segment.start) * 1000))
                    end_ms = max(start_ms + 1, round(float(segment.end) * 1000))
                    step = max(1, (end_ms - start_ms) // max(1, len(tokens)))
                    for index, token in enumerate(tokens):
                        result.append(
                            WordTiming(
                                word_id=f"{source_id}:{sequence}",
                                text=token,
                                model_text=token,
                                start_ms=start_ms + index * step,
                                end_ms=(
                                    end_ms
                                    if index == len(tokens) - 1
                                    else start_ms + (index + 1) * step
                                ),
                                source_id=source_id,
                            )
                        )
                        sequence += 1
            return result
        except WorkerError:
            raise
        except Exception as exc:
            raise _translate_ml_error(exc, "Whisper transcription failed.") from exc

    def transcribe_pcm(self, pcm: bytes, sample_rate: int, channels: int) -> str:
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as handle:
            temp_path = Path(handle.name)
        try:
            with wave.open(str(temp_path), "wb") as output:
                output.setnchannels(channels)
                output.setsampwidth(2)
                output.setframerate(sample_rate)
                output.writeframes(pcm)
            words = self.transcribe(temp_path, "live", threading.Event())
            return join_tokens([word.text for word in words])
        finally:
            temp_path.unlink(missing_ok=True)

    def release(self) -> None:
        self._model = None
        _release_cuda_cache()


class RetryingTranscriber:
    """Try CUDA FP16, CUDA int8_float16, then CPU int8 only on OOM."""

    def __init__(self, attempts: Sequence[Transcriber]) -> None:
        if not attempts:
            raise ValueError("At least one transcription backend is required.")
        self.attempts = tuple(attempts)

    def transcribe(
        self, audio_path: Path, source_id: str, cancel: threading.Event
    ) -> list[WordTiming]:
        last_error: WorkerError | None = None
        for backend in self.attempts:
            try:
                return backend.transcribe(audio_path, source_id, cancel)
            except WorkerError as exc:
                if exc.code is not ErrorCode.GPU_OUT_OF_MEMORY:
                    raise
                release_backend(backend)
                last_error = exc
        if last_error:
            raise last_error
        raise AssertionError("unreachable")

    def release(self) -> None:
        for backend in self.attempts:
            release_backend(backend)


class WhisperXAligner:
    def __init__(self, model_path: Path, *, device: str) -> None:
        self.model_path = model_path
        self.device = device
        self._model: Any = None
        self._metadata: Any = None
        self._load_lock = threading.Lock()

    def _load(self) -> tuple[Any, Any, Any]:
        try:
            import whisperx
        except ImportError as exc:
            raise missing_dependency("whisperx", "word alignment") from exc
        if self._model is None:
            with self._load_lock:
                if self._model is None:
                    self._model, self._metadata = whisperx.load_align_model(
                        language_code="en",
                        device=self.device,
                        model_name=str(self.model_path),
                        model_dir=str(self.model_path.parent),
                        model_cache_only=True,
                    )
        return whisperx, self._model, self._metadata

    def align(
        self,
        audio_path: Path,
        source_id: str,
        words: Sequence[WordTiming],
        cancel: threading.Event,
    ) -> list[WordTiming]:
        if not words:
            return []
        _cancelled(cancel)
        try:
            whisperx, model, metadata = self._load()
            segments = _words_to_alignment_segments(words)
            audio = whisperx.load_audio(str(audio_path))
            with _offline_whisperx_sentence_tokenizer():
                aligned = whisperx.align(
                    segments,
                    model,
                    metadata,
                    audio,
                    self.device,
                    return_char_alignments=False,
                )
            output: list[WordTiming] = []
            for index, item in enumerate(aligned.get("word_segments", [])):
                if "start" not in item or "end" not in item:
                    continue
                text = str(item.get("word", "")).strip()
                if not text:
                    continue
                output.append(
                    WordTiming(
                        word_id=f"{source_id}:{index}",
                        text=text,
                        model_text=text,
                        start_ms=max(0, round(float(item["start"]) * 1000)),
                        end_ms=max(0, round(float(item["end"]) * 1000)),
                        source_id=source_id,
                        confidence=(
                            float(item["score"]) if item.get("score") is not None else None
                        ),
                    )
                )
            return output or list(words)
        except WorkerError:
            raise
        except Exception as exc:
            raise _translate_ml_error(exc, "WhisperX alignment failed.") from exc

    def release(self) -> None:
        self._model = None
        self._metadata = None
        _release_cuda_cache()


class RetryingAligner:
    """Retry WhisperX on CPU only when the CUDA attempt exhausts device memory."""

    def __init__(
        self,
        attempts: Sequence[Aligner],
        *,
        on_fallback: FallbackCallback | None = None,
    ) -> None:
        if not attempts:
            raise ValueError("At least one alignment backend is required.")
        self.attempts = tuple(attempts)
        self.on_fallback = on_fallback

    def align(
        self,
        audio_path: Path,
        source_id: str,
        words: Sequence[WordTiming],
        cancel: threading.Event,
    ) -> list[WordTiming]:
        last_error: WorkerError | None = None
        for index, backend in enumerate(self.attempts):
            try:
                return backend.align(audio_path, source_id, words, cancel)
            except WorkerError as exc:
                if exc.code is not ErrorCode.GPU_OUT_OF_MEMORY:
                    raise
                release_backend(backend)
                last_error = exc
                if index + 1 < len(self.attempts) and self.on_fallback is not None:
                    self.on_fallback("align", exc)
        if last_error is not None:
            raise last_error
        raise AssertionError("unreachable")

    def release(self) -> None:
        for backend in self.attempts:
            release_backend(backend)


def _words_to_alignment_segments(words: Sequence[WordTiming]) -> list[dict[str, Any]]:
    segments: list[dict[str, Any]] = []
    current: list[WordTiming] = []
    for word in words:
        if current and word.start_ms - current[-1].end_ms > 1_500:
            segments.append(
                {
                    "start": current[0].start_ms / 1000,
                    "end": current[-1].end_ms / 1000,
                    "text": join_tokens([value.text for value in current]),
                }
            )
            current = []
        current.append(word)
    if current:
        segments.append(
            {
                "start": current[0].start_ms / 1000,
                "end": current[-1].end_ms / 1000,
                "text": join_tokens([value.text for value in current]),
            }
        )
    return segments


@contextmanager
def _offline_whisperx_sentence_tokenizer() -> Iterator[None]:
    """Keep WhisperX sentence grouping local when optional NLTK data is absent."""

    alignment = cast(Any, importlib.import_module("whisperx.alignment"))
    try:
        from nltk.tokenize import PunktSentenceTokenizer
    except ImportError as exc:
        raise missing_dependency("nltk", "word alignment") from exc
    original_load = alignment.nltk_load

    def load_local(resource: str) -> Any:
        return _load_offline_sentence_tokenizer(resource, original_load, PunktSentenceTokenizer)

    with _WHISPERX_SENTENCE_TOKENIZER_LOCK:
        alignment.nltk_load = load_local
        try:
            yield
        finally:
            alignment.nltk_load = original_load


def _load_offline_sentence_tokenizer(
    resource: str, loader: Callable[[str], Any], fallback_type: Callable[[], Any]
) -> Any:
    try:
        return loader(resource)
    except LookupError:
        if resource.startswith("tokenizers/punkt_tab/"):
            return fallback_type()
        raise


class PyannoteDiarizer:
    def __init__(self, model_path: Path, *, device: str) -> None:
        self.model_path = model_path
        self.device = device
        self._pipeline: Any = None
        self._load_lock = threading.Lock()

    def _load(self) -> Any:
        if self._pipeline is not None:
            return self._pipeline
        with self._load_lock:
            if self._pipeline is None:
                try:
                    import torch
                    from pyannote.audio import Pipeline
                except ImportError as exc:
                    raise missing_dependency("pyannote.audio", "speaker diarization") from exc
                try:
                    self._pipeline = Pipeline.from_pretrained(str(self.model_path))
                    if self.device == "cuda":
                        self._pipeline.to(torch.device("cuda"))
                except Exception as exc:
                    raise _translate_ml_error(exc, "Unable to load Community-1.") from exc
        return self._pipeline

    def diarize(self, audio_path: Path, cancel: threading.Event) -> list[DiarizationSegment]:
        _cancelled(cancel)
        try:
            output = self._load()(_pyannote_waveform(audio_path))
            regular = output.speaker_diarization
            exclusive = getattr(output, "exclusive_speaker_diarization", regular)
            overlap_intervals = _overlap_intervals(regular)
            result: list[DiarizationSegment] = []
            for segment, label in _annotation_segments(exclusive):
                start_ms = round(float(segment.start) * 1000)
                end_ms = round(float(segment.end) * 1000)
                result.append(
                    DiarizationSegment(
                        start_ms=start_ms,
                        end_ms=end_ms,
                        speaker_cluster_id=str(label),
                        overlap=any(
                            max(start_ms, left) < min(end_ms, right)
                            for left, right in overlap_intervals
                        ),
                    )
                )
            return result
        except WorkerError:
            raise
        except Exception as exc:
            raise _translate_ml_error(exc, "Speaker diarization failed.") from exc

    def release(self) -> None:
        self._pipeline = None
        _release_cuda_cache()


class RetryingDiarizer:
    """Retry Community-1 on CPU only after a CUDA out-of-memory failure."""

    def __init__(
        self,
        attempts: Sequence[Diarizer],
        *,
        on_fallback: FallbackCallback | None = None,
    ) -> None:
        if not attempts:
            raise ValueError("At least one diarization backend is required.")
        self.attempts = tuple(attempts)
        self.on_fallback = on_fallback

    def diarize(self, audio_path: Path, cancel: threading.Event) -> list[DiarizationSegment]:
        last_error: WorkerError | None = None
        for index, backend in enumerate(self.attempts):
            try:
                return backend.diarize(audio_path, cancel)
            except WorkerError as exc:
                if exc.code is not ErrorCode.GPU_OUT_OF_MEMORY:
                    raise
                release_backend(backend)
                last_error = exc
                if index + 1 < len(self.attempts) and self.on_fallback is not None:
                    self.on_fallback("diarize", exc)
        if last_error is not None:
            raise last_error
        raise AssertionError("unreachable")

    def release(self) -> None:
        for backend in self.attempts:
            release_backend(backend)


def _annotation_segments(annotation: Any) -> list[tuple[Any, str]]:
    if hasattr(annotation, "itertracks"):
        return [(segment, str(label)) for segment, _track, label in annotation.itertracks(True)]
    return [(segment, str(label)) for segment, label in annotation]


def _overlap_intervals(annotation: Any) -> list[tuple[int, int]]:
    tracks = _annotation_segments(annotation)
    points: list[tuple[int, int]] = []
    for segment, _label in tracks:
        points.append((round(float(segment.start) * 1000), 1))
        points.append((round(float(segment.end) * 1000), -1))
    points.sort(key=lambda value: (value[0], value[1]))
    result: list[tuple[int, int]] = []
    active = 0
    overlap_start: int | None = None
    for timestamp, delta in points:
        previous = active
        active += delta
        if previous < 2 <= active:
            overlap_start = timestamp
        elif previous >= 2 > active and overlap_start is not None:
            result.append((overlap_start, timestamp))
            overlap_start = None
    return result


class WeSpeakerEmbedder:
    def __init__(self, model_path: Path, *, device: str) -> None:
        self.model_path = model_path
        self.device = device
        self._inference: Any = None
        self._segment_type: Any = None
        self._load_lock = threading.Lock()

    def _load(self) -> tuple[Any, Any]:
        if self._inference is not None:
            return self._inference, self._segment_type
        with self._load_lock:
            if self._inference is None:
                try:
                    import torch
                    from pyannote.audio import Inference, Model
                    from pyannote.core import Segment
                except ImportError as exc:
                    raise missing_dependency("pyannote.audio", "speaker embeddings") from exc
                model = Model.from_pretrained(str(self.model_path))
                if self.device == "cuda":
                    model.to(torch.device("cuda"))  # type: ignore[union-attr]
                self._inference = Inference(model, window="whole")  # type: ignore[arg-type]
                self._segment_type = Segment
        return self._inference, self._segment_type

    def embed_intervals(
        self,
        audio_path: Path,
        intervals_ms: Sequence[tuple[int, int]],
        cancel: threading.Event,
    ) -> SpeakerEmbeddingBatch:
        result: list[tuple[float, ...]] = []
        clean_duration_ms = 0
        try:
            inference, segment_type = self._load()
            audio = _pyannote_waveform(audio_path)
            sample_count = int(audio["waveform"].shape[-1])
            sample_rate = int(audio["sample_rate"])
            for start_ms, end_ms in _clamp_intervals_to_audio(
                intervals_ms, sample_count=sample_count, sample_rate=sample_rate
            ):
                _cancelled(cancel)
                raw = inference.crop(audio, segment_type(start_ms / 1000, end_ms / 1000))
                values = cast(Any, raw).reshape(-1).tolist()
                if values and all(math.isfinite(float(value)) for value in values):
                    result.append(tuple(float(value) for value in values))
                    clean_duration_ms += end_ms - start_ms
            return SpeakerEmbeddingBatch(tuple(result), clean_duration_ms)
        except WorkerError:
            raise
        except Exception as exc:
            raise _translate_ml_error(exc, "Speaker embedding extraction failed.") from exc

    def release(self) -> None:
        self._inference = None
        self._segment_type = None
        _release_cuda_cache()


def _clamp_intervals_to_audio(
    intervals_ms: Sequence[tuple[int, int]], *, sample_count: int, sample_rate: int
) -> list[tuple[int, int]]:
    if sample_count < 0 or sample_rate <= 0:
        raise ValueError("Audio extent must have a positive sample rate and sample count.")
    # Keep the exclusive crop endpoint at least one sample inside the waveform.
    # Pyannote compares floating-point seconds and can reject an endpoint that
    # prints equal to the duration but rounds a fraction above it.
    duration_ms = max(0, (sample_count - 1) * 1000 // sample_rate)
    result: list[tuple[int, int]] = []
    for raw_start, raw_end in intervals_ms:
        start_ms = min(duration_ms, max(0, int(raw_start)))
        end_ms = min(duration_ms, max(start_ms, int(raw_end)))
        if end_ms - start_ms >= 750:
            result.append((start_ms, end_ms))
    return result


def _pyannote_waveform(audio_path: Path) -> dict[str, Any]:
    """Load normalized PCM without relying on TorchCodec or system FFmpeg discovery."""

    try:
        import numpy as np
        import torch
    except ImportError as exc:
        raise missing_dependency("torch", "speaker processing") from exc

    try:
        with wave.open(str(audio_path), "rb") as handle:
            channels = handle.getnchannels()
            sample_width = handle.getsampwidth()
            sample_rate = handle.getframerate()
            frame_count = handle.getnframes()
            if channels < 1 or sample_width != 2 or sample_rate < 1:
                raise ValueError("expected PCM s16le audio")
            pcm = handle.readframes(frame_count)
    except (OSError, EOFError, wave.Error, ValueError) as exc:
        raise WorkerError(
            ErrorCode.UNSUPPORTED_AUDIO,
            "Normalized audio could not be loaded for speaker processing.",
            {"path": str(audio_path)},
        ) from exc

    values = np.frombuffer(pcm, dtype="<i2").astype(np.float32)
    if channels > 1:
        values = values.reshape(-1, channels).mean(axis=1)
    values = values / 32768.0
    waveform = torch.from_numpy(values.copy()).unsqueeze(0)
    return {"waveform": waveform, "sample_rate": sample_rate}


class RetryingEmbedder:
    """Retry speaker embedding extraction on CPU after CUDA OOM."""

    def __init__(
        self,
        attempts: Sequence[Embedder],
        *,
        on_fallback: FallbackCallback | None = None,
    ) -> None:
        if not attempts:
            raise ValueError("At least one embedding backend is required.")
        self.attempts = tuple(attempts)
        self.on_fallback = on_fallback

    def embed_intervals(
        self,
        audio_path: Path,
        intervals_ms: Sequence[tuple[int, int]],
        cancel: threading.Event,
    ) -> SpeakerEmbeddingBatch:
        last_error: WorkerError | None = None
        for index, backend in enumerate(self.attempts):
            try:
                return backend.embed_intervals(audio_path, intervals_ms, cancel)
            except WorkerError as exc:
                if exc.code is not ErrorCode.GPU_OUT_OF_MEMORY:
                    raise
                release_backend(backend)
                last_error = exc
                if index + 1 < len(self.attempts) and self.on_fallback is not None:
                    self.on_fallback("identify", exc)
        if last_error is not None:
            raise last_error
        raise AssertionError("unreachable")

    def release(self) -> None:
        for backend in self.attempts:
            release_backend(backend)


class NumpyWaveCorrelation:
    """Calculate lag-tolerant correlation between normalized mono WAV word windows."""

    def __init__(self, source_paths: dict[str, Path], *, maximum_lag_ms: int = 180) -> None:
        self.source_paths = source_paths
        self.maximum_lag_ms = maximum_lag_ms

    def __call__(self, left: WordTiming, right: WordTiming) -> float | None:
        try:
            import numpy as np
        except ImportError:
            return None
        left_audio = _read_wave_window(
            self.source_paths[left.source_id], left.start_ms - 120, left.end_ms + 120, np
        )
        right_audio = _read_wave_window(
            self.source_paths[right.source_id], right.start_ms - 120, right.end_ms + 120, np
        )
        if left_audio is None or right_audio is None:
            return None
        left_samples, left_rate = left_audio
        right_samples, right_rate = right_audio
        if left_rate != right_rate or not len(left_samples) or not len(right_samples):
            return None
        size = min(len(left_samples), len(right_samples))
        left_samples = left_samples[:size]
        right_samples = right_samples[:size]
        left_samples = left_samples - left_samples.mean()
        right_samples = right_samples - right_samples.mean()
        maximum_lag = round(left_rate * self.maximum_lag_ms / 1000)
        step = max(1, left_rate // 200)
        best = -1.0
        for lag in range(-maximum_lag, maximum_lag + 1, step):
            if lag < 0:
                one, two = left_samples[-lag:], right_samples[: size + lag]
            elif lag > 0:
                one, two = left_samples[: size - lag], right_samples[lag:]
            else:
                one, two = left_samples, right_samples
            if len(one) < left_rate // 10:
                continue
            denominator = float(np.linalg.norm(one) * np.linalg.norm(two))
            if denominator > 1e-9:
                best = max(best, float(np.dot(one, two) / denominator))
        return best if best >= -0.5 else None


def _read_wave_window(path: Path, start_ms: int, end_ms: int, np: Any) -> tuple[Any, int] | None:
    with wave.open(str(path), "rb") as handle:
        if handle.getsampwidth() != 2:
            return None
        sample_rate = handle.getframerate()
        start_frame = max(0, round(start_ms * sample_rate / 1000))
        end_frame = min(handle.getnframes(), round(end_ms * sample_rate / 1000))
        if end_frame <= start_frame:
            return None
        handle.setpos(start_frame)
        values = np.frombuffer(handle.readframes(end_frame - start_frame), dtype="<i2").astype(
            np.float32
        )
        channels = handle.getnchannels()
        if channels > 1:
            values = values.reshape(-1, channels).mean(axis=1)
        return values, sample_rate


def _translate_ml_error(exc: Exception, message: str) -> WorkerError:
    text = str(exc).casefold()
    if "out of memory" in text or "cuda_error_out_of_memory" in text:
        return WorkerError(
            ErrorCode.GPU_OUT_OF_MEMORY,
            message,
            {"exception": type(exc).__name__},
            retryable=True,
        )
    return WorkerError(
        ErrorCode.INTERNAL,
        message,
        {"exception": type(exc).__name__},
        retryable=False,
    )
