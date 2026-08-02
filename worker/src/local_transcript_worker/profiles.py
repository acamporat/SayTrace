"""Conservative known-speaker matching.

The host decrypts DPAPI-protected profile vectors only for the duration of a job.
This module never writes or returns those vectors.
"""

from __future__ import annotations

import math
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any

from .errors import ErrorCode, WorkerError
from .schema import JsonObject, require_string

MIN_PROFILE_SAMPLES = 1
MIN_PROFILE_CLEAN_DURATION_MS = 10_000


@dataclass(frozen=True, slots=True)
class VoiceProfile:
    profile_id: str
    name: str
    embeddings: tuple[tuple[float, ...], ...]
    sample_durations_ms: tuple[int, ...]
    explicitly_confirmed: bool

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> VoiceProfile:
        raw_embeddings = data.get("embeddings")
        raw_durations = data.get("sample_durations_ms")
        if not isinstance(raw_embeddings, list) or not isinstance(raw_durations, list):
            raise WorkerError(ErrorCode.BAD_REQUEST, "Invalid voice profile samples.")
        try:
            embeddings = tuple(tuple(float(value) for value in row) for row in raw_embeddings)
            durations = tuple(int(value) for value in raw_durations)
        except (TypeError, ValueError) as exc:
            raise WorkerError(ErrorCode.BAD_REQUEST, "Invalid voice profile vectors.") from exc
        if not embeddings or len(embeddings) != len(durations):
            raise WorkerError(ErrorCode.BAD_REQUEST, "Voice profile sample metadata is incomplete.")
        dimension = len(embeddings[0])
        if (
            dimension == 0
            or dimension > 2_048
            or len(embeddings) > 64
            or any(len(row) != dimension for row in embeddings)
        ):
            raise WorkerError(
                ErrorCode.BAD_REQUEST,
                "Voice profile vectors have invalid dimensions or sample count.",
            )
        return cls(
            profile_id=require_string(data.get("profile_id"), "profile_id"),
            name=require_string(data.get("name"), "name"),
            embeddings=embeddings,
            sample_durations_ms=durations,
            explicitly_confirmed=bool(data.get("explicitly_confirmed", False)),
        )

    @property
    def eligible(self) -> bool:
        return (
            self.explicitly_confirmed
            and len(self.embeddings) >= MIN_PROFILE_SAMPLES
            and sum(self.sample_durations_ms) >= MIN_PROFILE_CLEAN_DURATION_MS
            and all(duration > 0 for duration in self.sample_durations_ms)
        )

    def centroid(self) -> tuple[float, ...]:
        return normalized(
            tuple(sum(values) / len(values) for values in zip(*self.embeddings, strict=True))
        )


@dataclass(frozen=True, slots=True)
class MatchPolicy:
    """Thresholds must be validated against the application's consented benchmark."""

    calibration_id: str
    accept_similarity: float
    accept_margin: float
    review_similarity: float

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> MatchPolicy:
        try:
            result = cls(
                calibration_id=require_string(data.get("calibration_id"), "calibration_id"),
                accept_similarity=float(data["accept_similarity"]),
                accept_margin=float(data["accept_margin"]),
                review_similarity=float(data["review_similarity"]),
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise WorkerError(ErrorCode.BAD_REQUEST, "Invalid speaker match policy.") from exc
        if not (
            0 <= result.review_similarity <= result.accept_similarity <= 1
            and 0 <= result.accept_margin <= 1
        ):
            raise WorkerError(ErrorCode.BAD_REQUEST, "Speaker thresholds must be within [0, 1].")
        return result


@dataclass(frozen=True, slots=True)
class SpeakerMatch:
    profile_id: str | None
    name: str
    state: str
    similarity: float | None
    runner_up_margin: float | None
    calibration_id: str

    def as_dict(self) -> JsonObject:
        return {
            "profile_id": self.profile_id,
            "name": self.name,
            "state": self.state,
            "similarity": self.similarity,
            "runner_up_margin": self.runner_up_margin,
            "calibration_id": self.calibration_id,
        }


@dataclass(frozen=True, slots=True)
class SpeakerEmbeddingBatch:
    embeddings: tuple[tuple[float, ...], ...]
    clean_duration_ms: int


def normalized(vector: Sequence[float]) -> tuple[float, ...]:
    magnitude = math.sqrt(sum(value * value for value in vector))
    if magnitude <= 1e-12 or not math.isfinite(magnitude):
        raise WorkerError(ErrorCode.BAD_REQUEST, "Speaker embedding has zero or invalid magnitude.")
    return tuple(value / magnitude for value in vector)


def cosine(left: Sequence[float], right: Sequence[float]) -> float:
    if len(left) != len(right):
        raise WorkerError(ErrorCode.BAD_REQUEST, "Speaker embedding dimensions do not match.")
    left_norm = normalized(left)
    right_norm = normalized(right)
    return max(-1.0, min(1.0, sum(a * b for a, b in zip(left_norm, right_norm, strict=True))))


def average_embeddings(embeddings: Iterable[Sequence[float]]) -> tuple[float, ...]:
    rows = [tuple(float(value) for value in embedding) for embedding in embeddings]
    if not rows:
        raise WorkerError(ErrorCode.BAD_REQUEST, "No clean speaker embeddings were produced.")
    dimension = len(rows[0])
    if dimension == 0 or any(len(row) != dimension for row in rows):
        raise WorkerError(ErrorCode.BAD_REQUEST, "Cluster embeddings have mixed dimensions.")
    return normalized(tuple(sum(values) / len(values) for values in zip(*rows, strict=True)))


def match_speaker(
    cluster_embedding: Sequence[float],
    profiles: Sequence[VoiceProfile],
    policy: MatchPolicy,
) -> SpeakerMatch:
    eligible = [profile for profile in profiles if profile.eligible]
    if not eligible:
        return SpeakerMatch(None, "Unknown", "Unknown", None, None, policy.calibration_id)
    ranked = sorted(
        ((cosine(cluster_embedding, profile.centroid()), profile) for profile in eligible),
        key=lambda pair: (-pair[0], pair[1].profile_id),
    )
    best_score, best = ranked[0]
    runner_up = ranked[1][0] if len(ranked) > 1 else -1.0
    margin = best_score - runner_up
    if best_score >= policy.accept_similarity and margin >= policy.accept_margin:
        return SpeakerMatch(
            best.profile_id,
            best.name,
            "Matched",
            best_score,
            margin,
            policy.calibration_id,
        )
    if best_score >= policy.review_similarity:
        return SpeakerMatch(
            best.profile_id,
            best.name,
            "Review",
            best_score,
            margin,
            policy.calibration_id,
        )
    return SpeakerMatch(
        None,
        "Unknown",
        "Unknown",
        best_score,
        margin,
        policy.calibration_id,
    )
