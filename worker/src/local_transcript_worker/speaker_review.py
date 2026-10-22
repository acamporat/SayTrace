"""Conservative final checks for isolated diarization boundary fragments.

Duration alone is never evidence that two voices are the same. Only repair a
fragment inside a single aligned word, with stronger combined support from the
same speaker on both sides. Keep all other short speakers for human review.
"""

from collections import Counter, defaultdict
from collections.abc import Sequence
from dataclasses import replace

from .schema import DiarizationSegment, JsonObject, WordTiming


def review_speaker_fragments(
    words: Sequence[WordTiming], segments: Sequence[DiarizationSegment]
) -> tuple[list[WordTiming], list[JsonObject]]:
    ordered = sorted(segments, key=lambda segment: (segment.start_ms, segment.end_ms))
    counts = Counter(segment.speaker_cluster_id for segment in ordered)
    by_cluster: dict[str, list[WordTiming]] = defaultdict(list)
    for word in words:
        if word.speaker_cluster_id:
            by_cluster[word.speaker_cluster_id].append(word)
    replacements: dict[str, str] = {}
    warnings: list[JsonObject] = []
    for index, segment in enumerate(ordered):
        cluster = segment.speaker_cluster_id
        duration = segment.end_ms - segment.start_ms
        affected = by_cluster.get(cluster, [])
        if counts[cluster] != 1 or not 0 < duration < 750 or not affected:
            continue
        target: str | None = None
        if 0 < index < len(ordered) - 1 and duration <= 350 and not segment.overlap:
            before, after = ordered[index - 1], ordered[index + 1]
            if (
                before.speaker_cluster_id == after.speaker_cluster_id
                and before.speaker_cluster_id != cluster
                and not before.overlap
                and not after.overlap
                and 0 <= segment.start_ms - before.end_ms <= 80
                and 0 <= after.start_ms - segment.end_ms <= 80
                and len(affected) == 1
            ):
                word = affected[0]
                left = max(0, min(word.end_ms, before.end_ms) - max(word.start_ms, before.start_ms))
                right = max(0, min(word.end_ms, after.end_ms) - max(word.start_ms, after.start_ms))
                if (
                    not word.overlap
                    and word.start_ms < segment.start_ms
                    and word.end_ms > segment.end_ms
                    and left > 0
                    and right > 0
                    and left + right >= 1.5 * duration
                    and left + right >= 0.6 * (word.end_ms - word.start_ms)
                ):
                    target = before.speaker_cluster_id
        if target is not None:
            replacements[cluster] = target
        warnings.append(
            {
                "code": "SPEAKER_FRAGMENT_REPAIRED" if target else "SHORT_SPEAKER_REVIEW",
                "cluster": cluster,
                "start_ms": segment.start_ms,
                "end_ms": segment.end_ms,
                "replacement_cluster": target,
                "message": (
                    "A brief speaker boundary inside one word was reconciled "
                    "with its surrounding speaker."
                    if target
                    else "Brief speaker retained: insufficient evidence to merge; review the audio."
                ),
            }
        )
    return [
        replace(word, speaker_cluster_id=replacements[word.speaker_cluster_id])
        if word.speaker_cluster_id in replacements
        else word
        for word in words
    ], warnings
