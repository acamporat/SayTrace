from __future__ import annotations

from local_transcript_worker.live import LiveDraftManager, LiveStream, LocalAgreement
from local_transcript_worker.schema import AudioMetadata


def test_local_agreement_commits_only_repeated_prefix() -> None:
    agreement = LocalAgreement()

    first = agreement.update("hello brave")
    second = agreement.update("hello brave new")
    third = agreement.update("hello brave new world")

    assert first["committed_text"] == ""
    assert first["unstable_text"] == "hello brave"
    assert second["committed_text"] == "hello brave"
    assert second["unstable_text"] == "new"
    assert third["committed_text"] == "hello brave new"
    assert third["unstable_text"] == "world"


def test_live_window_is_bounded_and_caption_backlog_is_coalesced() -> None:
    stream = LiveStream(
        "session",
        "mic",
        speaker_hint="You",
        sample_rate=16_000,
        channels=1,
        window_ms=2_000,
        backlog_limit_ms=1_000,
        decode_interval_ms=500,
    )
    pcm_750ms = b"\0\0" * 12_000
    for sequence in range(4):
        stream.push(
            AudioMetadata("session", "mic", sequence, sequence * 750, 16_000, 1, "s16le"),
            pcm_750ms,
        )

    snapshot = stream.snapshot_if_due()

    assert snapshot is not None
    pcm, coalesced = snapshot
    assert len(pcm) <= len(pcm_750ms) * 3
    assert coalesced > 0


def test_live_sequence_gap_reports_warning() -> None:
    stream = LiveStream("session", "mic", speaker_hint="You", sample_rate=16_000, channels=1)
    pcm = b"\0\0" * 160

    assert stream.push(AudioMetadata("session", "mic", 0, 0, 16_000, 1, "s16le"), pcm) is None
    warning = stream.push(AudioMetadata("session", "mic", 2, 20, 16_000, 1, "s16le"), pcm)

    assert warning is not None
    assert warning["code"] == "LIVE_AUDIO_SEQUENCE_GAP"


def test_last_live_session_releases_backend_before_final_processing() -> None:
    class FakeLiveBackend:
        def __init__(self) -> None:
            self.release_calls = 0

        def transcribe_pcm(self, pcm: bytes, sample_rate: int, channels: int) -> str:
            return ""

        def release(self) -> None:
            self.release_calls += 1

    backend = FakeLiveBackend()
    manager = LiveDraftManager(backend, lambda _event, _payload: None, poll_interval=1)
    manager.start_session("first", {"mic": "microphone"})
    manager.start_session("second", {"loopback": "loopback"})

    assert manager.stop_session("first") is False
    assert backend.release_calls == 0
    assert manager.active_session_count == 1

    assert manager.stop_session("second") is True
    assert backend.release_calls == 1
    assert manager.active_session_count == 0
