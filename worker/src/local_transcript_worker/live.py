"""Bounded live-caption buffering with LocalAgreement-2 style revisions."""

from __future__ import annotations

import re
import threading
import time
from collections import deque
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field

from .backends import LiveTranscriber, release_backend
from .errors import ErrorCode, WorkerError
from .schema import AudioMetadata, JsonObject

_TOKEN = re.compile(r"\S+")
EventCallback = Callable[[str, JsonObject], None]


def tokenize(text: str) -> list[str]:
    return _TOKEN.findall(text.strip())


def longest_common_prefix(left: list[str], right: list[str]) -> int:
    length = 0
    for one, two in zip(left, right, strict=False):
        if one.casefold() != two.casefold():
            break
        length += 1
    return length


@dataclass(slots=True)
class LocalAgreement:
    committed: list[str] = field(default_factory=list)
    previous_unstable: list[str] = field(default_factory=list)
    revision: int = 0

    def update(self, hypothesis: str, *, final: bool = False) -> JsonObject:
        current = self._strip_committed_overlap(tokenize(hypothesis))
        if final:
            agreed = current
            unstable: list[str] = []
        else:
            agreed_count = longest_common_prefix(self.previous_unstable, current)
            agreed = current[:agreed_count]
            unstable = current[agreed_count:]
        self.committed.extend(agreed)
        self.previous_unstable = unstable
        self.revision += 1
        return {
            "revision": self.revision,
            "replace_from_token": len(self.committed),
            "committed_text": " ".join(self.committed),
            "unstable_text": " ".join(unstable),
            "is_final": final,
        }

    def _strip_committed_overlap(self, current: list[str]) -> list[str]:
        maximum = min(len(self.committed), len(current))
        for length in range(maximum, 0, -1):
            committed_suffix = [value.casefold() for value in self.committed[-length:]]
            current_prefix = [value.casefold() for value in current[:length]]
            if committed_suffix == current_prefix:
                return current[length:]
        return current


@dataclass(frozen=True, slots=True)
class _Chunk:
    pcm: bytes
    duration_ms: int
    sequence: int


class LiveStream:
    def __init__(
        self,
        session_id: str,
        stream_id: str,
        *,
        speaker_hint: str,
        sample_rate: int,
        channels: int,
        window_ms: int = 25_000,
        decode_interval_ms: int = 1_000,
        backlog_limit_ms: int = 20_000,
    ) -> None:
        self.session_id = session_id
        self.stream_id = stream_id
        self.speaker_hint = speaker_hint
        self.sample_rate = sample_rate
        self.channels = channels
        self.window_ms = window_ms
        self.decode_interval_ms = decode_interval_ms
        self.backlog_limit_ms = backlog_limit_ms
        self.agreement = LocalAgreement()
        self._chunks: deque[_Chunk] = deque()
        self._window_duration_ms = 0
        self._undecoded_ms = 0
        self._last_sequence = -1
        self._coalesced_ms = 0
        self._lock = threading.Lock()

    def push(self, metadata: AudioMetadata, pcm: bytes) -> JsonObject | None:
        if metadata.sample_rate != self.sample_rate or metadata.channels != self.channels:
            raise WorkerError(
                ErrorCode.UNSUPPORTED_AUDIO,
                "Live stream audio format changed without restarting the stream.",
            )
        frame_count = len(pcm) // (metadata.channels * 2)
        duration_ms = round(frame_count * 1000 / metadata.sample_rate)
        warning: JsonObject | None = None
        with self._lock:
            if self._last_sequence >= 0 and metadata.sequence != self._last_sequence + 1:
                warning = {
                    "code": "LIVE_AUDIO_SEQUENCE_GAP",
                    "expected_sequence": self._last_sequence + 1,
                    "actual_sequence": metadata.sequence,
                }
            self._last_sequence = metadata.sequence
            self._chunks.append(_Chunk(pcm, duration_ms, metadata.sequence))
            self._window_duration_ms += duration_ms
            self._undecoded_ms += duration_ms
            while self._window_duration_ms > self.window_ms and self._chunks:
                removed = self._chunks.popleft()
                self._window_duration_ms -= removed.duration_ms
            if self._undecoded_ms > self.backlog_limit_ms:
                self._coalesced_ms += self._undecoded_ms - self.backlog_limit_ms
                self._undecoded_ms = self.backlog_limit_ms
        return warning

    def snapshot_if_due(self, *, force: bool = False) -> tuple[bytes, int] | None:
        with self._lock:
            if not force and self._undecoded_ms < self.decode_interval_ms:
                return None
            if not self._chunks:
                return None
            pcm = b"".join(chunk.pcm for chunk in self._chunks)
            coalesced = self._coalesced_ms
            self._coalesced_ms = 0
            self._undecoded_ms = 0
            return pcm, coalesced

    def apply_hypothesis(
        self, hypothesis: str, *, coalesced_ms: int = 0, final: bool = False
    ) -> JsonObject:
        revision = self.agreement.update(hypothesis, final=final)
        return {
            "session_id": self.session_id,
            "stream_id": self.stream_id,
            "speaker_hint": self.speaker_hint,
            "coalesced_audio_ms": coalesced_ms,
            **revision,
        }


class LiveDraftManager:
    """One disposable caption thread; no capture or lossless recording state lives here."""

    def __init__(
        self,
        backend: LiveTranscriber,
        emit: EventCallback,
        *,
        poll_interval: float = 0.1,
    ) -> None:
        self.backend = backend
        self.emit = emit
        self.poll_interval = poll_interval
        self._streams: dict[tuple[str, str], LiveStream] = {}
        self._stream_types: dict[tuple[str, str], str] = {}
        self._lock = threading.Lock()
        self._decode_lock = threading.Lock()
        self._close_lock = threading.Lock()
        self._stop = threading.Event()
        self._closed = False
        self._released = False
        self._thread = threading.Thread(target=self._run, name="live-caption", daemon=True)
        self._thread.start()

    @property
    def active_session_count(self) -> int:
        with self._lock:
            return len({session_id for session_id, _stream_id in self._stream_types})

    def start_session(self, session_id: str, stream_types: Mapping[str, str]) -> None:
        with self._lock:
            if self._closed:
                raise WorkerError(ErrorCode.BAD_REQUEST, "Live caption manager is closed.")
            if any(key[0] == session_id for key in self._stream_types):
                raise WorkerError(ErrorCode.BAD_REQUEST, "Live session already exists.")
            for stream_id, source_type in stream_types.items():
                if source_type not in {"microphone", "loopback", "mixed"}:
                    raise WorkerError(ErrorCode.BAD_REQUEST, "Invalid live stream type.")
                self._stream_types[(session_id, stream_id)] = source_type

    def push(self, metadata: AudioMetadata, pcm: bytes) -> None:
        key = (metadata.session_id, metadata.stream_id)
        with self._lock:
            source_type = self._stream_types.get(key)
            if source_type is None:
                raise WorkerError(ErrorCode.BAD_REQUEST, "Audio belongs to an unknown live stream.")
            stream = self._streams.get(key)
            if stream is None:
                stream = LiveStream(
                    metadata.session_id,
                    metadata.stream_id,
                    speaker_hint="You" if source_type == "microphone" else "Speaker",
                    sample_rate=metadata.sample_rate,
                    channels=metadata.channels,
                )
                self._streams[key] = stream
        warning = stream.push(metadata, pcm)
        if warning:
            self.emit("device_warning", {"session_id": metadata.session_id, **warning})

    def stop_session(self, session_id: str) -> bool:
        with self._lock:
            if not any(key[0] == session_id for key in self._stream_types):
                raise WorkerError(ErrorCode.BAD_REQUEST, "Live session does not exist.")
            streams = [stream for key, stream in self._streams.items() if key[0] == session_id]
        for stream in streams:
            self._decode(stream, force=True, final=True)
        with self._lock:
            session_keys = [key for key in self._stream_types if key[0] == session_id]
            for key in session_keys:
                self._streams.pop(key, None)
                self._stream_types.pop(key, None)
            last_session = not self._stream_types
        return self.close() if last_session else False

    def close(self) -> bool:
        with self._close_lock:
            with self._lock:
                if self._released:
                    return True
                self._closed = True
            self._stop.set()
            # Never release a model while the caption thread is inside inference.
            # A bounded failure leaves the manager resident so pipeline.run can
            # return WORKER_BUSY and retry cleanup instead of racing CUDA teardown.
            if not self._decode_lock.acquire(timeout=30):
                return False
            self._decode_lock.release()
            self._thread.join(timeout=1)
            if self._thread.is_alive():
                return False
            release_backend(self.backend)
            with self._lock:
                self._released = True
            return True

    def _run(self) -> None:
        while not self._stop.wait(self.poll_interval):
            with self._lock:
                streams = list(self._streams.values())
            for stream in streams:
                if self._stop.is_set():
                    break
                self._decode(stream)

    def _decode(self, stream: LiveStream, *, force: bool = False, final: bool = False) -> None:
        with self._decode_lock:
            snapshot = stream.snapshot_if_due(force=force)
            if snapshot is None:
                return
            pcm, coalesced_ms = snapshot
            started = time.monotonic()
            try:
                hypothesis = self.backend.transcribe_pcm(pcm, stream.sample_rate, stream.channels)
                payload = stream.apply_hypothesis(
                    hypothesis, coalesced_ms=coalesced_ms, final=final
                )
                payload["decode_ms"] = round((time.monotonic() - started) * 1000)
                self.emit("draft_revision", payload)
            except WorkerError as exc:
                self.emit(
                    "live_error",
                    {
                        "session_id": stream.session_id,
                        "stream_id": stream.stream_id,
                        "error": exc.as_dict(),
                    },
                )
            except Exception as exc:  # containment for the disposable draft path
                self.emit(
                    "live_error",
                    {
                        "session_id": stream.session_id,
                        "stream_id": stream.stream_id,
                        "error": {
                            "code": ErrorCode.INTERNAL.value,
                            "message": "Live caption decoding failed.",
                            "retryable": True,
                            "details": {"exception": type(exc).__name__},
                        },
                    },
                )
