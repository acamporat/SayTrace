from dataclasses import replace

import pytest

from local_transcript_worker.merge import assign_speakers
from local_transcript_worker.schema import DiarizationSegment, WordTiming
from local_transcript_worker.speaker_review import review_speaker_fragments


def fragment() -> list[DiarizationSegment]:
    return [
        DiarizationSegment(0, 400, "a"),
        DiarizationSegment(400, 700, "b"),
        DiarizationSegment(700, 2000, "a"),
    ]


def test_repairs_split_word_using_combined_surrounding_support() -> None:
    # Each surrounding slice loses to B individually in initial assignment.
    words = assign_speakers([WordTiming("1", "hello", 150, 950, "mix")], fragment(), {})
    assert words[0].speaker_cluster_id == "b"
    reviewed, warnings = review_speaker_fragments(words, fragment())
    assert reviewed[0] == replace(words[0], speaker_cluster_id="a")
    assert warnings[0]["code"] == "SPEAKER_FRAGMENT_REPAIRED"
    assert words[0].speaker_cluster_id == "b"  # preserve raw evidence
    assert review_speaker_fragments(reviewed, fragment())[0] == reviewed


@pytest.mark.parametrize("start,end", [(400, 700), (350, 750), (100, 650)])
def test_preserves_real_interjection_and_ambiguous_boundaries(start: int, end: int) -> None:
    words = [WordTiming("1", "yes", start, end, "mix", speaker_cluster_id="b")]
    reviewed, warnings = review_speaker_fragments(words, fragment())
    assert reviewed == words
    assert warnings[0]["code"] == "SHORT_SPEAKER_REVIEW"


@pytest.mark.parametrize("case", ["overlap", "gap", "different", "recurring", "isolated"])
def test_does_not_collapse_unsupported_fragments(case: str) -> None:
    segments = fragment()
    words = [WordTiming("1", "hello", 150, 950, "mix", speaker_cluster_id="b")]
    if case == "overlap":
        segments[1] = replace(segments[1], overlap=True)
    elif case == "gap":
        segments[0] = replace(segments[0], end_ms=300)
    elif case == "different":
        segments[2] = replace(segments[2], speaker_cluster_id="c")
    elif case == "recurring":
        segments.append(DiarizationSegment(2200, 3000, "b"))
    else:
        words[0] = replace(words[0], speaker_cluster_id="isolated:You")
    assert review_speaker_fragments(words, segments)[0] == words


def test_empty_input() -> None:
    assert review_speaker_fragments([], []) == ([], [])
