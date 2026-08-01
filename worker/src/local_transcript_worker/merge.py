"""Source-aware reconciliation, evidence-gated bleed removal, and turn grouping."""

from __future__ import annotations

import math
import re
import uuid
from collections import defaultdict
from collections.abc import Callable, Iterable, Mapping, Sequence

from .profiles import SpeakerMatch
from .schema import DiarizationSegment, TranscriptTurn, WordTiming

CorrelationProvider = Callable[[WordTiming, WordTiming], float | None]
_TOKEN = re.compile(r"[\w']+", re.UNICODE)


def normalized_token(text: str) -> str:
    match = _TOKEN.search(text.casefold())
    return match.group(0) if match else text.casefold().strip()


def overlap_ratio(left: WordTiming, right: WordTiming) -> float:
    overlap = max(0, min(left.end_ms, right.end_ms) - max(left.start_ms, right.start_ms))
    shortest = min(left.end_ms - left.start_ms, right.end_ms - right.start_ms)
    return overlap / shortest if shortest > 0 else 0.0


def assign_speakers(
    words: Sequence[WordTiming],
    segments: Sequence[DiarizationSegment],
    isolated_speakers: Mapping[str, str],
) -> list[WordTiming]:
    result: list[WordTiming] = []
    for word in words:
        isolated = isolated_speakers.get(word.source_id)
        if isolated:
            speaker = f"isolated:{isolated}"
            overlap = False
        else:
            scored = [
                (
                    max(0, min(word.end_ms, segment.end_ms) - max(word.start_ms, segment.start_ms)),
                    segment,
                )
                for segment in segments
            ]
            best_overlap, best = max(scored, key=lambda value: value[0], default=(0, None))
            speaker = best.speaker_cluster_id if best_overlap and best is not None else "unknown"
            overlap = bool(best and best.overlap)
        result.append(
            WordTiming(
                word_id=word.word_id,
                text=word.text,
                model_text=word.model_text,
                start_ms=word.start_ms,
                end_ms=word.end_ms,
                source_id=word.source_id,
                confidence=word.confidence,
                speaker_cluster_id=speaker,
                overlap=overlap,
                acoustic_correlation=word.acoustic_correlation,
                lexical_similarity=word.lexical_similarity,
            )
        )
    return result


def deduplicate_bleed(
    words: Sequence[WordTiming],
    source_priority: Mapping[str, int],
    *,
    correlation: CorrelationProvider | None = None,
    minimum_overlap: float = 0.55,
    minimum_correlation: float = 0.82,
) -> list[WordTiming]:
    """Remove only cross-source duplicates with timing *and* acoustic evidence.

    Lexical coincidence alone is deliberately insufficient, preserving real interruptions
    and same-word crosstalk.
    """

    ordered = sorted(words, key=lambda word: (word.start_ms, word.end_ms, word.word_id))
    removed: set[str] = set()
    for index, left in enumerate(ordered):
        if left.word_id in removed:
            continue
        for right in ordered[index + 1 :]:
            if right.start_ms > left.end_ms + 250:
                break
            if right.word_id in removed or left.source_id == right.source_id:
                continue
            if normalized_token(left.text) != normalized_token(right.text):
                continue
            if overlap_ratio(left, right) < minimum_overlap:
                continue
            acoustic = _evidence_correlation(left, right, correlation)
            lexical = min(
                left.lexical_similarity if left.lexical_similarity is not None else 1.0,
                right.lexical_similarity if right.lexical_similarity is not None else 1.0,
            )
            if acoustic is None or acoustic < minimum_correlation or lexical < 0.9:
                continue
            keep, discard = _preferred(left, right, source_priority)
            removed.add(discard.word_id)
            if keep.word_id != left.word_id:
                break
    return [word for word in ordered if word.word_id not in removed]


def _evidence_correlation(
    left: WordTiming,
    right: WordTiming,
    provider: CorrelationProvider | None,
) -> float | None:
    known = [
        value
        for value in (left.acoustic_correlation, right.acoustic_correlation)
        if value is not None and math.isfinite(value)
    ]
    if known:
        return max(known)
    return provider(left, right) if provider else None


def _preferred(
    left: WordTiming, right: WordTiming, source_priority: Mapping[str, int]
) -> tuple[WordTiming, WordTiming]:
    left_key = (
        source_priority.get(left.source_id, 0),
        left.confidence if left.confidence is not None else -1.0,
        -left.start_ms,
    )
    right_key = (
        source_priority.get(right.source_id, 0),
        right.confidence if right.confidence is not None else -1.0,
        -right.start_ms,
    )
    return (left, right) if left_key >= right_key else (right, left)


def group_turns(
    words: Sequence[WordTiming],
    matches: Mapping[str, SpeakerMatch],
    isolated_names: Mapping[str, str],
    *,
    turn_namespace: str,
    maximum_gap_ms: int = 1_200,
    maximum_words_per_turn: int = 500,
) -> list[TranscriptTurn]:
    turns: list[TranscriptTurn] = []
    current: list[WordTiming] = []
    current_speaker = ""
    meeting_namespace = uuid.uuid5(
        uuid.NAMESPACE_URL, f"local-transcript:meeting-job:{turn_namespace}"
    )

    def flush() -> None:
        if not current:
            return
        speaker = current_speaker
        if speaker.startswith("isolated:"):
            name = isolated_names.get(speaker, speaker.partition(":")[2])
            state = "Matched"
        else:
            match = matches.get(speaker)
            name = match.name if match else "Unknown"
            state = match.state if match else "Unknown"
        turns.append(
            TranscriptTurn(
                turn_id=str(
                    uuid.uuid5(
                        meeting_namespace,
                        f"{speaker}:{current[0].start_ms}:{current[-1].end_ms}",
                    )
                ),
                speaker_cluster_id=speaker,
                speaker_name=name,
                speaker_state=state,
                start_ms=current[0].start_ms,
                end_ms=current[-1].end_ms,
                model_text=join_tokens(word.text for word in current),
                words=tuple(current),
                needs_review=state != "Matched" or any(word.overlap for word in current),
            )
        )
        current.clear()

    for word in sorted(words, key=lambda value: (value.start_ms, value.end_ms, value.word_id)):
        speaker = word.speaker_cluster_id or "unknown"
        if current and (
            speaker != current_speaker
            or word.start_ms - current[-1].end_ms > maximum_gap_ms
            or len(current) >= maximum_words_per_turn
        ):
            flush()
        if not current:
            current_speaker = speaker
        current.append(word)
    flush()
    return turns


def join_tokens(tokens: Iterable[str]) -> str:
    values = list(tokens)
    result = ""
    no_space_before = set(".,!?;:%)]}")
    no_space_after = set("([{")
    for token in values:
        clean = str(token).strip()
        if not clean:
            continue
        if not result or clean[0] in no_space_before or result[-1] in no_space_after:
            result += clean
        else:
            result += " " + clean
    return result


def clusters_to_intervals(
    segments: Sequence[DiarizationSegment],
) -> dict[str, list[tuple[int, int]]]:
    intervals: dict[str, list[tuple[int, int]]] = defaultdict(list)
    for segment in segments:
        if not segment.overlap and segment.end_ms - segment.start_ms >= 750:
            intervals[segment.speaker_cluster_id].append((segment.start_ms, segment.end_ms))
    return dict(intervals)
