from __future__ import annotations

import threading

import pytest

from local_transcript_worker.resident import (
    ResidentBackendCache,
    resident_cache_policy,
)

GIBIBYTE = 1024**3


class FakeBackend:
    def __init__(self, name: str) -> None:
        self.name = name
        self.release_calls = 0

    def release(self) -> None:
        self.release_calls += 1


@pytest.mark.parametrize(
    ("memory_gib", "tier", "max_entries", "idle_seconds"),
    [
        (8, "low", 2, 120),
        (12, "balanced", 4, 300),
        (16, "standard", 6, 600),
        (32, "high", 8, 600),
    ],
)
def test_resident_policy_scales_with_unified_memory(
    memory_gib: int, tier: str, max_entries: int, idle_seconds: int
) -> None:
    policy = resident_cache_policy(enabled=True, detected_memory_bytes=memory_gib * GIBIBYTE)

    assert policy.memory_tier == tier
    assert policy.max_entries == max_entries
    assert policy.idle_seconds == idle_seconds
    assert policy.as_dict()["adaptive"] is True
    assert policy.stage_bounded_release is (memory_gib <= 16)
    assert policy.prewarm_components == (
        ("final_asr",) if memory_gib <= 16 else ("final_asr", "diarization", "speaker_embedding")
    )


def test_sixteen_gib_policy_bounds_the_unified_memory_working_set() -> None:
    policy = resident_cache_policy(enabled=True, detected_memory_bytes=16 * GIBIBYTE)

    # Keep the fast MLX ASR prewarm, but unload it before Pyannote uses MPS so
    # the two large accelerator working sets cannot exhaust unified memory.
    assert policy.max_entries >= 5
    assert policy.idle_seconds == 600
    assert policy.stage_bounded_release is True
    assert policy.prewarm_components == ("final_asr",)


def test_policy_retains_full_warm_set_only_above_sixteen_gib() -> None:
    policy = resident_cache_policy(
        enabled=True,
        detected_memory_bytes=16 * GIBIBYTE + 1,
    )

    assert policy.stage_bounded_release is False
    assert policy.prewarm_components == (
        "final_asr",
        "diarization",
        "speaker_embedding",
    )


def test_unknown_memory_policy_fails_safe(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        "local_transcript_worker.resident.physical_memory_bytes",
        lambda: None,
    )

    policy = resident_cache_policy(enabled=True)

    assert policy.memory_tier == "low"
    assert policy.stage_bounded_release is True
    assert policy.prewarm_components == ("final_asr",)


def test_resident_policy_preserves_explicit_test_overrides() -> None:
    policy = resident_cache_policy(
        enabled=True,
        detected_memory_bytes=8 * GIBIBYTE,
        idle_seconds=42,
        max_entries=7,
    )

    assert policy.memory_tier == "low"
    assert policy.idle_seconds == 42
    assert policy.max_entries == 7
    assert policy.as_dict()["adaptive"] is False
    assert policy.idle_overridden is True
    assert policy.max_entries_overridden is True


def test_resident_cache_reuses_backend_until_idle_deadline() -> None:
    now = [100.0]
    created: list[FakeBackend] = []

    def factory() -> FakeBackend:
        backend = FakeBackend("asr")
        created.append(backend)
        return backend

    cache = ResidentBackendCache(
        idle_seconds=10,
        clock=lambda: now[0],
        start_reaper=False,
    )

    first = cache.acquire("final_asr:mlx", factory)
    first_value = first.value
    first.close()
    now[0] = 105.0
    second = cache.acquire("final_asr:mlx", factory)

    assert second.value is first_value
    assert len(created) == 1
    second.close()
    now[0] = 114.9
    assert cache.evict_idle(now=now[0]) == ()
    now[0] = 115.0
    assert cache.evict_idle(now=now[0]) == ("final_asr:mlx",)
    assert first_value.release_calls == 1
    cache.close()


def test_resident_cache_never_evicts_an_active_lease() -> None:
    cache = ResidentBackendCache(idle_seconds=10, start_reaper=False)
    backend = FakeBackend("diarization")
    lease = cache.acquire("diarization:mps", lambda: backend)

    assert cache.evict_idle(force=True) == ()
    assert backend.release_calls == 0
    assert cache.close() == ()
    assert backend.release_calls == 0

    lease.close()

    assert backend.release_calls == 1


def test_resident_cache_enforces_lru_entry_bound() -> None:
    lifecycle_lock = threading.RLock()
    cache = ResidentBackendCache(
        idle_seconds=60,
        max_entries=2,
        lifecycle_lock=lifecycle_lock,
        start_reaper=False,
    )
    one = FakeBackend("one")
    two = FakeBackend("two")
    three = FakeBackend("three")
    cache.acquire("one", lambda: one).close()
    cache.acquire("two", lambda: two).close()

    cache.acquire("three", lambda: three).close()

    assert len(cache.keys()) == 2
    assert one.release_calls == 1
    assert two.release_calls == 0
    assert three.release_calls == 0
    cache.close()
    assert two.release_calls == 1
    assert three.release_calls == 1


def test_resident_cache_reaper_releases_after_idle_timeout() -> None:
    released = threading.Event()

    class SignalingBackend(FakeBackend):
        def release(self) -> None:
            super().release()
            released.set()

    cache = ResidentBackendCache(idle_seconds=0.02)
    backend = SignalingBackend("asr")
    cache.acquire("final_asr:mlx", lambda: backend).close()

    assert released.wait(timeout=1)
    assert backend.release_calls == 1
    assert cache.keys() == ()
    cache.close()
