"""Protocol and pipeline data validation without a runtime schema dependency."""

from __future__ import annotations

import re
from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from . import PROTOCOL_VERSION
from .errors import ErrorCode, WorkerError

JsonObject = dict[str, Any]
_IDENTIFIER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")


def require_string(value: Any, field_name: str, *, allow_empty: bool = False) -> str:
    if not isinstance(value, str) or (not allow_empty and not value.strip()):
        raise WorkerError(ErrorCode.BAD_REQUEST, f"'{field_name}' must be a non-empty string.")
    return value


def require_object(value: Any, field_name: str) -> JsonObject:
    if not isinstance(value, dict) or any(not isinstance(key, str) for key in value):
        raise WorkerError(ErrorCode.BAD_REQUEST, f"'{field_name}' must be a JSON object.")
    return value


def require_identifier(value: Any, field_name: str) -> str:
    result = require_string(value, field_name)
    if not _IDENTIFIER.fullmatch(result):
        raise WorkerError(
            ErrorCode.BAD_REQUEST,
            f"'{field_name}' contains unsupported characters or is too long.",
        )
    return result


@dataclass(frozen=True, slots=True)
class Request:
    request_id: str
    command: str
    payload: JsonObject
    protocol_version: str = PROTOCOL_VERSION
    job_id: str | None = None
    pipeline_version: str | None = None

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> Request:
        version = require_string(data.get("protocol_version"), "protocol_version")
        if version != PROTOCOL_VERSION:
            raise WorkerError(
                ErrorCode.PROTOCOL_MISMATCH,
                f"Host protocol {version!r} is incompatible with worker {PROTOCOL_VERSION!r}.",
                {"host": version, "worker": PROTOCOL_VERSION},
            )
        if data.get("type") != "request":
            raise WorkerError(ErrorCode.BAD_REQUEST, "'type' must be 'request'.")
        job_id = data.get("job_id")
        pipeline_version = data.get("pipeline_version")
        return cls(
            request_id=require_identifier(data.get("request_id"), "request_id"),
            command=require_string(data.get("command"), "command"),
            payload=require_object(data.get("payload", {}), "payload"),
            protocol_version=version,
            job_id=require_identifier(job_id, "job_id") if job_id is not None else None,
            pipeline_version=(
                require_string(pipeline_version, "pipeline_version")
                if pipeline_version is not None
                else None
            ),
        )


@dataclass(frozen=True, slots=True)
class AudioMetadata:
    session_id: str
    stream_id: str
    sequence: int
    start_ms: int
    sample_rate: int
    channels: int
    sample_format: str

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> AudioMetadata:
        try:
            result = cls(
                session_id=require_identifier(data.get("session_id"), "session_id"),
                stream_id=require_identifier(data.get("stream_id"), "stream_id"),
                sequence=int(data["sequence"]),
                start_ms=int(data["start_ms"]),
                sample_rate=int(data["sample_rate"]),
                channels=int(data["channels"]),
                sample_format=require_string(data.get("sample_format"), "sample_format"),
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise WorkerError(ErrorCode.BAD_REQUEST, "Invalid audio frame metadata.") from exc
        if result.sequence < 0 or result.start_ms < 0:
            raise WorkerError(
                ErrorCode.BAD_REQUEST, "Audio sequence and start_ms must be non-negative."
            )
        if result.sample_rate not in {16_000, 44_100, 48_000}:
            raise WorkerError(ErrorCode.UNSUPPORTED_AUDIO, "Unsupported audio sample rate.")
        if result.channels not in {1, 2} or result.sample_format != "s16le":
            raise WorkerError(
                ErrorCode.UNSUPPORTED_AUDIO,
                "Live captions currently accept mono/stereo signed 16-bit little-endian PCM.",
            )
        return result


@dataclass(frozen=True, slots=True)
class SourceAsset:
    asset_id: str
    path: Path
    source_type: str
    priority: int = 0
    isolated_speaker: str | None = None

    @classmethod
    def from_dict(cls, data: Mapping[str, Any], path: Path) -> SourceAsset:
        source_type = require_string(data.get("source_type"), "source_type")
        if source_type not in {"microphone", "loopback", "import", "mixed"}:
            raise WorkerError(ErrorCode.BAD_REQUEST, f"Unsupported source_type {source_type!r}.")
        isolated = data.get("isolated_speaker")
        default_priority = {
            "loopback": 30,
            "mixed": 20,
            "import": 20,
            "microphone": 10,
        }[source_type]
        return cls(
            asset_id=require_identifier(data.get("asset_id"), "asset_id"),
            path=path,
            source_type=source_type,
            priority=int(data.get("priority", default_priority)),
            isolated_speaker=(
                require_string(isolated, "isolated_speaker") if isolated is not None else None
            ),
        )


@dataclass(frozen=True, slots=True)
class WordTiming:
    word_id: str
    text: str
    start_ms: int
    end_ms: int
    source_id: str
    confidence: float | None = None
    speaker_cluster_id: str | None = None
    overlap: bool = False
    acoustic_correlation: float | None = None
    lexical_similarity: float | None = None
    model_text: str | None = None

    def as_dict(self) -> JsonObject:
        return {
            "word_id": self.word_id,
            "text": self.text,
            "model_text": self.model_text if self.model_text is not None else self.text,
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
            "source_id": self.source_id,
            "confidence": self.confidence,
            "speaker_cluster_id": self.speaker_cluster_id,
            "overlap": self.overlap,
            "acoustic_correlation": self.acoustic_correlation,
            "lexical_similarity": self.lexical_similarity,
        }


@dataclass(frozen=True, slots=True)
class DiarizationSegment:
    start_ms: int
    end_ms: int
    speaker_cluster_id: str
    overlap: bool = False

    def as_dict(self) -> JsonObject:
        return {
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
            "speaker_cluster_id": self.speaker_cluster_id,
            "overlap": self.overlap,
        }


@dataclass(frozen=True, slots=True)
class TranscriptTurn:
    turn_id: str
    speaker_cluster_id: str
    speaker_name: str
    speaker_state: str
    start_ms: int
    end_ms: int
    model_text: str
    words: tuple[WordTiming, ...]
    needs_review: bool = False

    def as_dict(self) -> JsonObject:
        return {
            "turn_id": self.turn_id,
            "speaker_cluster_id": self.speaker_cluster_id,
            "speaker_name": self.speaker_name,
            "speaker_state": self.speaker_state,
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
            "model_text": self.model_text,
            "text": self.model_text,
            "needs_review": self.needs_review,
            "words": [word.as_dict() for word in self.words],
        }


@dataclass(slots=True)
class PipelineCheckpoint:
    pipeline_version: str
    stage_results: dict[str, Any] = field(default_factory=dict)
    completed_batches: set[str] = field(default_factory=set)

    @classmethod
    def from_dict(cls, data: Mapping[str, Any], pipeline_version: str) -> PipelineCheckpoint:
        if data and data.get("pipeline_version") not in {None, pipeline_version}:
            raise WorkerError(
                ErrorCode.BAD_REQUEST,
                "Checkpoint was created by a different pipeline version.",
            )
        raw_batches = data.get("completed_batches", [])
        raw_results = data.get("stage_results", {})
        if not isinstance(raw_batches, list) or not isinstance(raw_results, dict):
            raise WorkerError(ErrorCode.BAD_REQUEST, "Invalid checkpoint shape.")
        return cls(
            pipeline_version=pipeline_version,
            stage_results=dict(raw_results),
            completed_batches={str(value) for value in raw_batches},
        )

    def as_dict(self) -> JsonObject:
        return {
            "pipeline_version": self.pipeline_version,
            "completed_batches": sorted(self.completed_batches),
            "stage_results": self.stage_results,
        }
