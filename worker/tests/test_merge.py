from __future__ import annotations

from local_transcript_worker.merge import deduplicate_bleed, group_turns
from local_transcript_worker.schema import WordTiming


def _word(word_id: str, source: str, *, correlation: float | None) -> WordTiming:
    return WordTiming(
        word_id=word_id,
        text="hello",
        model_text="hello",
        start_ms=1000,
        end_ms=1400,
        source_id=source,
        confidence=0.9,
        acoustic_correlation=correlation,
        lexical_similarity=1.0,
    )


def test_confirmed_bleed_prefers_loopback_priority() -> None:
    mic = _word("mic:0", "mic", correlation=0.95)
    loopback = _word("loop:0", "loopback", correlation=0.95)

    result = deduplicate_bleed([mic, loopback], {"mic": 10, "loopback": 30})

    assert [word.word_id for word in result] == ["loop:0"]


def test_same_word_crosstalk_is_kept_without_acoustic_evidence() -> None:
    mic = _word("mic:0", "mic", correlation=None)
    loopback = _word("loop:0", "loopback", correlation=None)

    result = deduplicate_bleed([mic, loopback], {"mic": 10, "loopback": 30})

    assert {word.word_id for word in result} == {"mic:0", "loop:0"}


def test_turn_ids_are_stable_within_job_and_namespaced_across_jobs() -> None:
    word = _word("import:0", "import", correlation=None)
    word = WordTiming(
        word_id=word.word_id,
        text=word.text,
        model_text=word.model_text,
        start_ms=word.start_ms,
        end_ms=word.end_ms,
        source_id=word.source_id,
        speaker_cluster_id="speaker-0",
    )

    first = group_turns([word], {}, {}, turn_namespace="job-one")[0].turn_id
    repeated = group_turns([word], {}, {}, turn_namespace="job-one")[0].turn_id
    other_meeting = group_turns([word], {}, {}, turn_namespace="job-two")[0].turn_id

    assert first == repeated
    assert first != other_meeting
