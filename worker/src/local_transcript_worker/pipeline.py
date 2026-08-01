"""Resumable, source-aware canonical transcription pipeline."""

from __future__ import annotations

import base64
import json
import math
import os
import struct
import threading
import uuid
import wave
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .backends import (
    Aligner,
    Diarizer,
    Embedder,
    Normalizer,
    NumpyWaveCorrelation,
    Transcriber,
    release_backend,
)
from .errors import ErrorCode, WorkerError
from .merge import (
    assign_speakers,
    clusters_to_intervals,
    deduplicate_bleed,
    group_turns,
)
from .profiles import (
    MatchPolicy,
    SpeakerEmbeddingBatch,
    SpeakerMatch,
    VoiceProfile,
    average_embeddings,
    match_speaker,
)
from .schema import (
    DiarizationSegment,
    JsonObject,
    PipelineCheckpoint,
    SourceAsset,
    WordTiming,
)

PIPELINE_STAGES = (
    "normalize",
    "transcribe",
    "align",
    "diarize",
    "merge",
    "identify",
    "index",
    "finalize",
)
EventCallback = Callable[[str, JsonObject], None]
CorrelationFactory = Callable[[dict[str, Path]], Callable[[WordTiming, WordTiming], float | None]]
_MAX_SPEAKER_CANDIDATES = 128
_MAX_EMBEDDING_DIMENSION = 2_048
_MAX_SPEAKER_CANDIDATE_BYTES = 1_500_000


@dataclass(frozen=True, slots=True)
class PipelineInput:
    job_id: str
    pipeline_version: str
    sources: tuple[SourceAsset, ...]
    workspace: Path
    diarization_asset_id: str
    profiles: tuple[VoiceProfile, ...]
    match_policy: MatchPolicy | None
    checkpoint: PipelineCheckpoint


@dataclass(frozen=True, slots=True)
class _AsrChunk:
    source_id: str
    index: int
    audio_path: Path
    audio_start_ms: int
    core_start_ms: int
    core_end_ms: int
    final_chunk: bool


@dataclass(frozen=True, slots=True)
class _SpeakerCandidate:
    cluster_label: str
    clean_duration_ms: int
    embedding: tuple[float, ...]


class ArtifactStore:
    """Atomic JSON artifacts; every file is regenerable and host-approved."""

    def __init__(self, workspace: Path) -> None:
        self.root = workspace / "worker-artifacts"
        self.root.mkdir(parents=True, exist_ok=True)

    def write(self, stage: str, batch_id: str, data: JsonObject) -> Path:
        stage_root = self.root / stage
        stage_root.mkdir(parents=True, exist_ok=True)
        safe_name = uuid.uuid5(uuid.NAMESPACE_URL, batch_id).hex
        target = stage_root / f"{safe_name}.json"
        partial = target.with_suffix(".json.partial")
        raw = json.dumps(data, ensure_ascii=False, allow_nan=False, separators=(",", ":"))
        with partial.open("w", encoding="utf-8", newline="\n") as handle:
            handle.write(raw)
            handle.flush()
            os.fsync(handle.fileno())
        _fsync_directory(stage_root)
        partial.replace(target)
        _fsync_directory(stage_root)
        return target

    @staticmethod
    def read(path: Path) -> JsonObject:
        try:
            data: Any = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise WorkerError(
                ErrorCode.BAD_REQUEST,
                "A resume artifact is missing or corrupt.",
                {"path": str(path)},
            ) from exc
        if not isinstance(data, dict):
            raise WorkerError(ErrorCode.BAD_REQUEST, "Resume artifact must contain an object.")
        return data


class FinalPipeline:
    def __init__(
        self,
        *,
        normalizer: Normalizer,
        transcriber: Transcriber,
        aligner: Aligner | None,
        diarizer: Diarizer,
        embedder: Embedder | None,
        emit: EventCallback,
        correlation_factory: CorrelationFactory | None = None,
    ) -> None:
        self.normalizer = normalizer
        self.transcriber = transcriber
        self.aligner = aligner
        self.diarizer = diarizer
        self.embedder = embedder
        self.emit = emit
        self.correlation_factory = correlation_factory or NumpyWaveCorrelation

    def run(self, request: PipelineInput, cancel: threading.Event) -> JsonObject:
        store = ArtifactStore(request.workspace)
        checkpoint = request.checkpoint
        warnings: list[JsonObject] = []

        normalized = self._normalize(request, checkpoint, store, cancel)
        try:
            transcribed = self._transcribe(request, normalized, checkpoint, store, cancel)
        finally:
            release_backend(self.transcriber)
        try:
            aligned = self._align(
                request, normalized, transcribed, checkpoint, store, cancel, warnings
            )
        finally:
            release_backend(self.aligner)
        try:
            diarization = self._diarize(request, normalized, checkpoint, store, cancel)
        finally:
            release_backend(self.diarizer)
        merged = self._merge(request, normalized, aligned, diarization, checkpoint, store, cancel)
        try:
            matches, speaker_candidates = self._identify(
                request,
                normalized,
                diarization,
                checkpoint,
                store,
                cancel,
                warnings,
            )
        finally:
            release_backend(self.embedder)
        final_artifact, turns = self._index_and_finalize(
            request, merged, matches, checkpoint, store, warnings
        )
        self._progress(
            request,
            "finalize",
            "complete",
            len(turns),
            len(turns),
            checkpoint,
            artifact=final_artifact,
        )
        result: JsonObject = {
            "job_id": request.job_id,
            "pipeline_version": request.pipeline_version,
            "canonical_artifact_path": str(final_artifact),
            "turn_count": len(turns),
            "word_count": len(merged),
            "warnings": warnings,
            "draft_independent": True,
            "speaker_candidates": _serialize_speaker_candidates(speaker_candidates),
        }
        playback_artifact = _select_playback_artifact(request, normalized)
        if playback_artifact is not None:
            result["playback_artifact_path"] = str(playback_artifact)
        return result

    def _normalize(
        self,
        request: PipelineInput,
        checkpoint: PipelineCheckpoint,
        store: ArtifactStore,
        cancel: threading.Event,
    ) -> dict[str, Path]:
        result: dict[str, Path] = {}
        total = len(request.sources)
        for index, source in enumerate(request.sources, 1):
            batch = f"normalize:{source.asset_id}"
            resumed = self._resume(checkpoint, store, batch)
            if resumed:
                path = _workspace_file(request.workspace, str(resumed["normalized_path"]))
            else:
                path = request.workspace / "normalized" / f"{source.asset_id}.wav"
                self.normalizer.normalize(source.path, path, cancel)
                artifact = store.write(
                    "normalize",
                    batch,
                    {"asset_id": source.asset_id, "normalized_path": str(path)},
                )
                self._complete(checkpoint, batch, artifact)
            result[source.asset_id] = path
            self._progress(request, "normalize", "running", index, total, checkpoint)
        return result

    def _transcribe(
        self,
        request: PipelineInput,
        normalized: Mapping[str, Path],
        checkpoint: PipelineCheckpoint,
        store: ArtifactStore,
        cancel: threading.Event,
    ) -> dict[str, list[WordTiming]]:
        result: dict[str, list[WordTiming]] = {source.asset_id: [] for source in request.sources}
        plans = [
            chunk
            for source in request.sources
            for chunk in _plan_asr_chunks(
                source.asset_id, normalized[source.asset_id], request.workspace
            )
        ]
        total = len(plans)
        for progress_index, chunk in enumerate(plans, 1):
            batch = f"transcribe:{chunk.source_id}:{chunk.index:06d}"
            resumed = self._resume(checkpoint, store, batch)
            if resumed:
                words = [_word_from_dict(item) for item in resumed.get("words", [])]
            else:
                raw_words = self.transcriber.transcribe(chunk.audio_path, chunk.source_id, cancel)
                words = _offset_and_clip_words(raw_words, chunk)
                artifact = store.write(
                    "transcribe",
                    batch,
                    {
                        "asset_id": chunk.source_id,
                        "chunk_index": chunk.index,
                        "audio_start_ms": chunk.audio_start_ms,
                        "core_start_ms": chunk.core_start_ms,
                        "core_end_ms": chunk.core_end_ms,
                        "words": [word.as_dict() for word in words],
                    },
                )
                self._complete(checkpoint, batch, artifact)
            result[chunk.source_id].extend(words)
            self._progress(
                request,
                "transcribe",
                "running",
                progress_index,
                total,
                checkpoint,
                batch_payload={
                    "asset_id": chunk.source_id,
                    "chunk_index": chunk.index,
                    "word_count": len(words),
                },
            )
        for words in result.values():
            words.sort(key=lambda word: (word.start_ms, word.end_ms, word.word_id))
        return result

    def _align(
        self,
        request: PipelineInput,
        normalized: Mapping[str, Path],
        transcribed: Mapping[str, list[WordTiming]],
        checkpoint: PipelineCheckpoint,
        store: ArtifactStore,
        cancel: threading.Event,
        warnings: list[JsonObject],
    ) -> dict[str, list[WordTiming]]:
        result: dict[str, list[WordTiming]] = {}
        total = len(request.sources)
        for index, source in enumerate(request.sources, 1):
            batch = f"align:{source.asset_id}"
            resumed = self._resume(checkpoint, store, batch)
            if resumed:
                words = [_word_from_dict(item) for item in resumed.get("words", [])]
            else:
                degraded = False
                try:
                    words = (
                        self.aligner.align(
                            normalized[source.asset_id],
                            source.asset_id,
                            transcribed[source.asset_id],
                            cancel,
                        )
                        if self.aligner
                        else list(transcribed[source.asset_id])
                    )
                    degraded = self.aligner is None
                except WorkerError as exc:
                    if exc.code is ErrorCode.CANCELLED:
                        raise
                    degraded = True
                    words = list(transcribed[source.asset_id])
                    warnings.append(
                        {
                            "code": "ALIGNMENT_FALLBACK",
                            "asset_id": source.asset_id,
                            "error": exc.as_dict(),
                        }
                    )
                artifact = store.write(
                    "align",
                    batch,
                    {
                        "asset_id": source.asset_id,
                        "degraded": degraded,
                        "words": [word.as_dict() for word in words],
                    },
                )
                self._complete(checkpoint, batch, artifact)
            result[source.asset_id] = words
            self._progress(request, "align", "running", index, total, checkpoint)
        return result

    def _diarize(
        self,
        request: PipelineInput,
        normalized: Mapping[str, Path],
        checkpoint: PipelineCheckpoint,
        store: ArtifactStore,
        cancel: threading.Event,
    ) -> list[DiarizationSegment]:
        batch = f"diarize:{request.diarization_asset_id}"
        resumed = self._resume(checkpoint, store, batch)
        if resumed:
            segments = [
                DiarizationSegment(
                    start_ms=int(item["start_ms"]),
                    end_ms=int(item["end_ms"]),
                    speaker_cluster_id=str(item["speaker_cluster_id"]),
                    overlap=bool(item.get("overlap", False)),
                )
                for item in resumed.get("segments", [])
            ]
        else:
            segments = self.diarizer.diarize(normalized[request.diarization_asset_id], cancel)
            artifact = store.write(
                "diarize",
                batch,
                {"segments": [segment.as_dict() for segment in segments]},
            )
            self._complete(checkpoint, batch, artifact)
        self._progress(request, "diarize", "running", 1, 1, checkpoint)
        return segments

    def _merge(
        self,
        request: PipelineInput,
        normalized: Mapping[str, Path],
        aligned: Mapping[str, list[WordTiming]],
        diarization: Sequence[DiarizationSegment],
        checkpoint: PipelineCheckpoint,
        store: ArtifactStore,
        cancel: threading.Event,
    ) -> list[WordTiming]:
        if cancel.is_set():
            raise WorkerError(ErrorCode.CANCELLED, "Job was cancelled.")
        batch = "merge:all-sources"
        resumed = self._resume(checkpoint, store, batch)
        if resumed:
            merged = [_word_from_dict(item) for item in resumed.get("words", [])]
        else:
            flat = [word for source_words in aligned.values() for word in source_words]
            isolated_by_source = {
                source.asset_id: source.isolated_speaker
                for source in request.sources
                if source.isolated_speaker
            }
            assigned = assign_speakers(flat, diarization, isolated_by_source)
            correlation = self.correlation_factory(dict(normalized))
            merged = deduplicate_bleed(
                assigned,
                {source.asset_id: source.priority for source in request.sources},
                correlation=correlation,
            )
            artifact = store.write("merge", batch, {"words": [word.as_dict() for word in merged]})
            self._complete(checkpoint, batch, artifact)
        self._progress(request, "merge", "running", 1, 1, checkpoint)
        return merged

    def _identify(
        self,
        request: PipelineInput,
        normalized: Mapping[str, Path],
        diarization: Sequence[DiarizationSegment],
        checkpoint: PipelineCheckpoint,
        store: ArtifactStore,
        cancel: threading.Event,
        warnings: list[JsonObject],
    ) -> tuple[dict[str, SpeakerMatch], list[_SpeakerCandidate]]:
        batch = "identify:all-clusters"
        resumed = self._resume(checkpoint, store, batch)
        intervals = clusters_to_intervals(diarization)
        if resumed:
            matches = {
                cluster: _match_from_dict(value)
                for cluster, value in dict(resumed.get("matches", {})).items()
            }
        else:
            matches = {}
        candidates: list[_SpeakerCandidate] = []
        if self.embedder:
            for cluster, clean_intervals in intervals.items():
                try:
                    batch_result = self.embedder.embed_intervals(
                        normalized[request.diarization_asset_id],
                        clean_intervals,
                        cancel,
                    )
                    candidate = _speaker_candidate(cluster, batch_result)
                    if candidate is not None:
                        candidates.append(candidate)
                        if not resumed and request.profiles and request.match_policy is not None:
                            matches[cluster] = match_speaker(
                                candidate.embedding,
                                request.profiles,
                                request.match_policy,
                            )
                except WorkerError as exc:
                    if exc.code is ErrorCode.CANCELLED:
                        raise
                    warnings.append(
                        {
                            "code": "SPEAKER_IDENTIFICATION_FALLBACK",
                            "cluster": cluster,
                            "error": exc.as_dict(),
                        }
                    )
        if request.profiles and (self.embedder is None or request.match_policy is None):
            warnings.append(
                {
                    "code": "SPEAKER_IDENTIFICATION_DISABLED",
                    "message": (
                        "Profiles were supplied without an embedder and calibrated policy."
                    ),
                }
            )
        if not resumed:
            artifact = store.write(
                "identify",
                batch,
                {"matches": {cluster: match.as_dict() for cluster, match in matches.items()}},
            )
            self._complete(checkpoint, batch, artifact)
        self._progress(
            request,
            "identify",
            "running",
            len(intervals),
            max(1, len(intervals)),
            checkpoint,
        )
        return matches, candidates

    def _index_and_finalize(
        self,
        request: PipelineInput,
        merged: Sequence[WordTiming],
        matches: Mapping[str, SpeakerMatch],
        checkpoint: PipelineCheckpoint,
        store: ArtifactStore,
        warnings: Sequence[JsonObject],
    ) -> tuple[Path, list[JsonObject]]:
        isolated_names = {
            f"isolated:{source.isolated_speaker}": source.isolated_speaker
            for source in request.sources
            if source.isolated_speaker
        }
        complete_matches = _complete_speaker_matches(
            merged,
            matches,
            isolated_names,
            calibration_id=(
                request.match_policy.calibration_id
                if request.match_policy is not None
                else "not-configured"
            ),
        )
        serialized_matches = {
            cluster: _speaker_match_for_artifact(match)
            for cluster, match in sorted(complete_matches.items())
        }
        turns = [
            turn.as_dict()
            for turn in group_turns(
                merged,
                complete_matches,
                isolated_names,
                turn_namespace=request.job_id,
            )
        ]
        offset = 0
        for turn_batch in _bounded_turn_batches(turns):
            self.emit(
                "pipeline_batch",
                {
                    "job_id": request.job_id,
                    "stage": "index",
                    "offset": offset,
                    "turns": turn_batch,
                },
            )
            offset += len(turn_batch)
        index_batch = "index:canonical"
        index_artifact = store.write(
            "index",
            index_batch,
            {
                "pipeline_version": request.pipeline_version,
                "immutable_model_output": True,
                "speaker_matches": serialized_matches,
                "turns": turns,
            },
        )
        self._complete(checkpoint, index_batch, index_artifact)
        self._progress(request, "index", "running", len(turns), len(turns), checkpoint)

        final_batch = "finalize:canonical"
        final_artifact = store.write(
            "finalize",
            final_batch,
            {
                "schema_version": 1,
                "pipeline_version": request.pipeline_version,
                "immutable_model_output": True,
                "draft_independent": True,
                "speaker_confidence_kind": "categorical",
                "speaker_matches": serialized_matches,
                "turns": turns,
                "warnings": list(warnings),
            },
        )
        self._complete(checkpoint, final_batch, final_artifact)
        return final_artifact, turns

    def _resume(
        self, checkpoint: PipelineCheckpoint, store: ArtifactStore, batch_id: str
    ) -> JsonObject | None:
        if batch_id not in checkpoint.completed_batches:
            return None
        raw_path = checkpoint.stage_results.get(batch_id)
        if not isinstance(raw_path, str):
            raise WorkerError(ErrorCode.BAD_REQUEST, "Checkpoint artifact path is missing.")
        path = Path(raw_path).resolve(strict=True)
        root = store.root.resolve(strict=True)
        if path != root and root not in path.parents:
            raise WorkerError(
                ErrorCode.INVALID_PATH,
                "Checkpoint artifact is outside the current job workspace.",
            )
        return ArtifactStore.read(path)

    @staticmethod
    def _complete(checkpoint: PipelineCheckpoint, batch_id: str, artifact: Path) -> None:
        checkpoint.completed_batches.add(batch_id)
        checkpoint.stage_results[batch_id] = str(artifact)

    def _progress(
        self,
        request: PipelineInput,
        stage: str,
        status: str,
        completed: int,
        total: int,
        checkpoint: PipelineCheckpoint,
        *,
        artifact: Path | None = None,
        batch_payload: JsonObject | None = None,
    ) -> None:
        payload: JsonObject = {
            "job_id": request.job_id,
            "pipeline_version": request.pipeline_version,
            "stage": stage,
            "status": status,
            "completed_batches": completed,
            "total_batches": total,
            "resume": checkpoint.as_dict(),
        }
        if artifact:
            payload["artifact_path"] = str(artifact)
        if batch_payload:
            payload["batch"] = batch_payload
        self.emit("job_progress", payload)


def _word_from_dict(data: Mapping[str, Any]) -> WordTiming:
    return WordTiming(
        word_id=str(data["word_id"]),
        text=str(data["text"]),
        model_text=str(data.get("model_text", data["text"])),
        start_ms=int(data["start_ms"]),
        end_ms=int(data["end_ms"]),
        source_id=str(data["source_id"]),
        confidence=float(data["confidence"]) if data.get("confidence") is not None else None,
        speaker_cluster_id=(
            str(data["speaker_cluster_id"]) if data.get("speaker_cluster_id") is not None else None
        ),
        overlap=bool(data.get("overlap", False)),
        acoustic_correlation=(
            float(data["acoustic_correlation"])
            if data.get("acoustic_correlation") is not None
            else None
        ),
        lexical_similarity=(
            float(data["lexical_similarity"])
            if data.get("lexical_similarity") is not None
            else None
        ),
    )


def _match_from_dict(data: Mapping[str, Any]) -> SpeakerMatch:
    return SpeakerMatch(
        profile_id=str(data["profile_id"]) if data.get("profile_id") is not None else None,
        name=str(data["name"]),
        state=str(data["state"]),
        similarity=float(data["similarity"]) if data.get("similarity") is not None else None,
        runner_up_margin=(
            float(data["runner_up_margin"]) if data.get("runner_up_margin") is not None else None
        ),
        calibration_id=str(data["calibration_id"]),
    )


def _complete_speaker_matches(
    words: Sequence[WordTiming],
    matches: Mapping[str, SpeakerMatch],
    isolated_names: Mapping[str, str],
    *,
    calibration_id: str,
) -> dict[str, SpeakerMatch]:
    complete = dict(matches)
    for word in words:
        cluster = word.speaker_cluster_id or "unknown"
        if cluster in complete:
            continue
        isolated_name = isolated_names.get(cluster)
        if isolated_name is not None:
            complete[cluster] = SpeakerMatch(
                profile_id=None,
                name=isolated_name,
                state="Matched",
                similarity=None,
                runner_up_margin=None,
                calibration_id="isolated-source",
            )
        else:
            complete[cluster] = SpeakerMatch(
                profile_id=None,
                name="Unknown",
                state="Unknown",
                similarity=None,
                runner_up_margin=None,
                calibration_id=calibration_id,
            )
    return complete


def _speaker_match_for_artifact(match: SpeakerMatch) -> JsonObject:
    """Persist attribution metadata without serializing any voice embedding."""

    return {
        "profile_id": match.profile_id,
        "name": match.name,
        "state": match.state,
        "similarity": match.similarity,
        "margin": match.runner_up_margin,
        "calibration_id": match.calibration_id,
    }


def _speaker_candidate(
    cluster_label: str, batch: SpeakerEmbeddingBatch
) -> _SpeakerCandidate | None:
    if batch.clean_duration_ms <= 0 or not batch.embeddings:
        return None
    embedding = average_embeddings(batch.embeddings)
    if not 1 <= len(embedding) <= _MAX_EMBEDDING_DIMENSION or any(
        not math.isfinite(value) for value in embedding
    ):
        raise WorkerError(
            ErrorCode.BAD_REQUEST,
            "Speaker candidate embedding is invalid.",
            {"cluster": cluster_label},
        )
    return _SpeakerCandidate(cluster_label, batch.clean_duration_ms, embedding)


def _serialize_speaker_candidates(
    candidates: Sequence[_SpeakerCandidate],
) -> list[JsonObject]:
    """Serialize transient vectors for the private terminal job result only."""

    result: list[JsonObject] = []
    used_bytes = 0
    ordered = sorted(
        candidates,
        key=lambda candidate: (-candidate.clean_duration_ms, candidate.cluster_label),
    )
    for candidate in ordered[:_MAX_SPEAKER_CANDIDATES]:
        if (
            candidate.clean_duration_ms <= 0
            or not 1 <= len(candidate.embedding) <= _MAX_EMBEDDING_DIMENSION
            or any(not math.isfinite(value) for value in candidate.embedding)
        ):
            continue
        raw = struct.pack(f"<{len(candidate.embedding)}f", *candidate.embedding)
        record: JsonObject = {
            "cluster_label": candidate.cluster_label,
            "clean_duration_ms": candidate.clean_duration_ms,
            "embedding_base64": base64.b64encode(raw).decode("ascii"),
        }
        record_bytes = len(
            json.dumps(record, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        )
        if used_bytes + record_bytes > _MAX_SPEAKER_CANDIDATE_BYTES:
            break
        result.append(record)
        used_bytes += record_bytes
    return result


def _plan_asr_chunks(
    source_id: str,
    audio_path: Path,
    workspace: Path,
    *,
    chunk_ms: int = 600_000,
    overlap_ms: int = 5_000,
) -> list[_AsrChunk]:
    """Build deterministic core ranges with audio overlap for crash-resumable ASR."""

    duration_ms = _wave_duration_ms(audio_path)
    if duration_ms is None or duration_ms <= chunk_ms:
        return [
            _AsrChunk(
                source_id=source_id,
                index=0,
                audio_path=audio_path,
                audio_start_ms=0,
                core_start_ms=0,
                core_end_ms=duration_ms if duration_ms is not None else 2**63 - 1,
                final_chunk=True,
            )
        ]

    chunk_root = workspace / "worker-artifacts" / "asr-audio"
    chunk_root.mkdir(parents=True, exist_ok=True)
    result: list[_AsrChunk] = []
    core_start = 0
    index = 0
    while core_start < duration_ms:
        core_end = min(duration_ms, core_start + chunk_ms)
        audio_start = max(0, core_start - overlap_ms)
        audio_end = min(duration_ms, core_end + overlap_ms)
        chunk_path = chunk_root / f"{source_id}-{index:06d}.wav"
        expected_duration = audio_end - audio_start
        actual_duration = _wave_duration_ms(chunk_path) if chunk_path.is_file() else None
        if actual_duration is None or abs(actual_duration - expected_duration) > 5:
            _write_wave_slice(audio_path, chunk_path, audio_start, audio_end)
        result.append(
            _AsrChunk(
                source_id=source_id,
                index=index,
                audio_path=chunk_path,
                audio_start_ms=audio_start,
                core_start_ms=core_start,
                core_end_ms=core_end,
                final_chunk=core_end == duration_ms,
            )
        )
        index += 1
        core_start = core_end
    return result


def _wave_duration_ms(path: Path) -> int | None:
    try:
        with wave.open(str(path), "rb") as handle:
            if handle.getframerate() <= 0:
                return None
            return round(handle.getnframes() * 1000 / handle.getframerate())
    except (FileNotFoundError, EOFError, wave.Error):
        return None


def _write_wave_slice(source: Path, target: Path, start_ms: int, end_ms: int) -> None:
    partial = target.with_suffix(".wav.partial")
    with wave.open(str(source), "rb") as reader:
        sample_rate = reader.getframerate()
        start_frame = max(0, round(start_ms * sample_rate / 1000))
        end_frame = min(reader.getnframes(), round(end_ms * sample_rate / 1000))
        reader.setpos(start_frame)
        with partial.open("wb") as raw_output:
            with wave.open(raw_output, "wb") as output:
                output.setparams(reader.getparams())
                remaining = max(0, end_frame - start_frame)
                while remaining:
                    block = reader.readframes(min(remaining, sample_rate * 10))
                    if not block:
                        break
                    output.writeframesraw(block)
                    remaining -= len(block) // (reader.getnchannels() * reader.getsampwidth())
            raw_output.flush()
            os.fsync(raw_output.fileno())
    _fsync_directory(target.parent)
    partial.replace(target)
    _fsync_directory(target.parent)


def _offset_and_clip_words(words: Sequence[WordTiming], chunk: _AsrChunk) -> list[WordTiming]:
    result: list[WordTiming] = []
    for index, word in enumerate(words):
        start_ms = max(0, word.start_ms + chunk.audio_start_ms)
        end_ms = max(start_ms, word.end_ms + chunk.audio_start_ms)
        midpoint = start_ms + (end_ms - start_ms) // 2
        if midpoint < chunk.core_start_ms:
            continue
        if not chunk.final_chunk and midpoint >= chunk.core_end_ms:
            continue
        result.append(
            WordTiming(
                word_id=f"{chunk.source_id}:{chunk.index}:{index}",
                text=word.text,
                model_text=word.model_text,
                start_ms=start_ms,
                end_ms=end_ms,
                source_id=chunk.source_id,
                confidence=word.confidence,
                speaker_cluster_id=word.speaker_cluster_id,
                overlap=word.overlap,
                acoustic_correlation=word.acoustic_correlation,
                lexical_similarity=word.lexical_similarity,
            )
        )
    return result


def _bounded_turn_batches(
    turns: Sequence[JsonObject], *, maximum_bytes: int = 1_500_000
) -> list[list[JsonObject]]:
    batches: list[list[JsonObject]] = []
    current: list[JsonObject] = []
    current_bytes = 2
    for turn in turns:
        encoded_bytes = len(
            json.dumps(turn, ensure_ascii=False, allow_nan=False, separators=(",", ":")).encode(
                "utf-8"
            )
        )
        if current and current_bytes + encoded_bytes + 1 > maximum_bytes:
            batches.append(current)
            current = []
            current_bytes = 2
        current.append(turn)
        current_bytes += encoded_bytes + 1
    if current:
        batches.append(current)
    return batches


def _workspace_file(workspace: Path, raw_path: str) -> Path:
    path = Path(raw_path).resolve(strict=True)
    root = workspace.resolve(strict=True)
    if path != root and root not in path.parents:
        raise WorkerError(
            ErrorCode.INVALID_PATH,
            "Resume output points outside the current job workspace.",
        )
    if not path.is_file():
        raise WorkerError(ErrorCode.INVALID_PATH, "Resume output is not a file.")
    return path


def _select_playback_artifact(
    request: PipelineInput, normalized: Mapping[str, Path]
) -> Path | None:
    """Choose only a host-approved normalized source suitable for local playback."""

    candidates = [source for source in request.sources if source.source_type == "mixed"]
    if not candidates:
        candidates = [source for source in request.sources if source.source_type == "import"]
    if not candidates:
        return None
    selected = next(
        (source for source in candidates if source.asset_id == request.diarization_asset_id),
        candidates[0],
    )
    path = normalized.get(selected.asset_id)
    if path is None:
        raise WorkerError(
            ErrorCode.INVALID_PATH,
            "Playback source is missing its normalized workspace artifact.",
        )
    return _workspace_file(request.workspace, str(path))


def _fsync_directory(path: Path) -> None:
    """Persist directory entries where the platform supports directory fsync."""

    if os.name == "nt":
        return
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
    descriptor = os.open(path, flags)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
