from __future__ import annotations

from local_transcript_worker.backends import (
    _clamp_intervals_to_audio,
    _load_offline_sentence_tokenizer,
)


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
