"""Thread-safe resident backend cache with bounded idle retention.

The cache owns backend objects, while short-lived leases protect them from
eviction during inference.  Model objects remain lazy: acquiring a lease does
not import an ML framework or load weights until the backend is used/prewarmed.
"""

from __future__ import annotations

import os
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from typing import cast

from .backends import release_backend

_GIBIBYTE = 1024**3


@dataclass(frozen=True, slots=True)
class ResidentCachePolicy:
    enabled: bool
    memory_tier: str
    physical_memory_bytes: int | None
    idle_seconds: float
    max_entries: int
    idle_overridden: bool
    max_entries_overridden: bool

    @property
    def stage_bounded_release(self) -> bool:
        return (
            not self.enabled
            or self.physical_memory_bytes is None
            or self.physical_memory_bytes <= 16 * _GIBIBYTE
        )

    @property
    def prewarm_components(self) -> tuple[str, ...]:
        if not self.enabled:
            return ()
        if self.stage_bounded_release:
            return ("final_asr",)
        return ("final_asr", "diarization", "speaker_embedding")

    def as_dict(self) -> dict[str, object]:
        return {
            "enabled": self.enabled,
            "memory_tier": self.memory_tier,
            "physical_memory_mib": (
                round(self.physical_memory_bytes / 1024**2)
                if self.physical_memory_bytes is not None
                else None
            ),
            "idle_timeout_ms": round(self.idle_seconds * 1000),
            "max_entries": self.max_entries,
            "stage_bounded_release": self.stage_bounded_release,
            "prewarm_components": list(self.prewarm_components),
            "adaptive": not self.idle_overridden and not self.max_entries_overridden,
            "idle_overridden": self.idle_overridden,
            "max_entries_overridden": self.max_entries_overridden,
        }


def physical_memory_bytes() -> int | None:
    """Return installed physical memory using only Python's standard library."""

    try:
        pages = int(os.sysconf("SC_PHYS_PAGES"))
        page_size = int(os.sysconf("SC_PAGE_SIZE"))
    except (AttributeError, OSError, TypeError, ValueError):
        return None
    total = pages * page_size
    return total if pages > 0 and page_size > 0 and total > 0 else None


def resident_cache_policy(
    *,
    enabled: bool,
    detected_memory_bytes: int | None = None,
    idle_seconds: float | None = None,
    max_entries: int | None = None,
) -> ResidentCachePolicy:
    """Choose a conservative unified-memory retention tier for Apple Silicon."""

    if detected_memory_bytes is not None and detected_memory_bytes <= 0:
        raise ValueError("detected_memory_bytes must be positive")
    memory: int | None
    if detected_memory_bytes is not None:
        memory = detected_memory_bytes
    elif enabled:
        memory = physical_memory_bytes()
    else:
        memory = None
    if not enabled:
        tier = "disabled"
        default_idle = 60.0
        default_entries = 1
    elif memory is None or memory <= 8 * _GIBIBYTE:
        tier = "low"
        default_idle = 120.0
        default_entries = 2
    elif memory < 16 * _GIBIBYTE:
        tier = "balanced"
        default_idle = 300.0
        default_entries = 4
    elif memory < 32 * _GIBIBYTE:
        tier = "standard"
        default_idle = 600.0
        default_entries = 6
    else:
        tier = "high"
        default_idle = 600.0
        default_entries = 8
    selected_idle = default_idle if idle_seconds is None else idle_seconds
    selected_entries = default_entries if max_entries is None else max_entries
    if selected_idle <= 0:
        raise ValueError("idle_seconds must be positive")
    if selected_entries < 1:
        raise ValueError("max_entries must be positive")
    return ResidentCachePolicy(
        enabled=enabled,
        memory_tier=tier,
        physical_memory_bytes=memory,
        idle_seconds=selected_idle,
        max_entries=selected_entries,
        idle_overridden=idle_seconds is not None,
        max_entries_overridden=max_entries is not None,
    )


@dataclass(slots=True)
class _Entry:
    value: object
    users: int
    last_used: float


class BackendLease[T]:
    """An idempotently releasable reference to one cached backend."""

    def __init__(self, cache: ResidentBackendCache, key: str, value: T) -> None:
        self._cache = cache
        self._key = key
        self.value = value
        self._closed = False
        self._lock = threading.Lock()

    def close(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
        self._cache._return(self._key)

    def __enter__(self) -> T:
        return self.value

    def __exit__(self, _exc_type: object, _exc: object, _traceback: object) -> None:
        self.close()


class ResidentBackendCache:
    """Keep a bounded set of heavy backends resident between worker jobs."""

    def __init__(
        self,
        *,
        idle_seconds: float = 600.0,
        max_entries: int = 8,
        lifecycle_lock: threading.RLock | None = None,
        clock: Callable[[], float] = time.monotonic,
        start_reaper: bool = True,
    ) -> None:
        if idle_seconds <= 0:
            raise ValueError("idle_seconds must be positive")
        if max_entries < 1:
            raise ValueError("max_entries must be positive")
        self.idle_seconds = idle_seconds
        self.max_entries = max_entries
        self._clock = clock
        self._lifecycle_lock = lifecycle_lock or threading.RLock()
        self._condition = threading.Condition(threading.Lock())
        self._entries: dict[str, _Entry] = {}
        self._closed = False
        self._reaper: threading.Thread | None = None
        if start_reaper:
            self._reaper = threading.Thread(
                target=self._reaper_loop,
                name="resident-model-reaper",
                daemon=True,
            )
            self._reaper.start()

    def acquire[T](self, key: str, factory: Callable[[], T]) -> BackendLease[T]:
        if not key:
            raise ValueError("cache key must not be empty")
        with self._lifecycle_lock:
            with self._condition:
                if self._closed:
                    raise RuntimeError("resident backend cache is closed")
                entry = self._entries.get(key)
                if entry is None:
                    self._make_room_locked()
                    entry = _Entry(factory(), 0, self._clock())
                    self._entries[key] = entry
                entry.users += 1
                entry.last_used = self._clock()
                self._condition.notify_all()
                return BackendLease(self, key, cast(T, entry.value))

    def evict_idle(
        self,
        *,
        force: bool = False,
        keys: set[str] | None = None,
        now: float | None = None,
    ) -> tuple[str, ...]:
        """Release idle entries, returning stable component keys that were evicted."""

        released: list[str] = []
        timestamp = self._clock() if now is None else now
        with self._lifecycle_lock:
            with self._condition:
                for key, entry in list(self._entries.items()):
                    if keys is not None and key not in keys:
                        continue
                    if entry.users:
                        continue
                    if not force and timestamp - entry.last_used < self.idle_seconds:
                        continue
                    self._entries.pop(key)
                    release_backend(entry.value)
                    released.append(key)
                self._condition.notify_all()
        return tuple(sorted(released))

    def status(self) -> dict[str, object]:
        with self._condition:
            now = self._clock()
            return {
                "idle_timeout_ms": round(self.idle_seconds * 1000),
                "max_entries": self.max_entries,
                "entries": [
                    {
                        "component": key,
                        "in_use": entry.users > 0,
                        "idle_ms": 0
                        if entry.users
                        else max(0, round((now - entry.last_used) * 1000)),
                    }
                    for key, entry in sorted(self._entries.items())
                ],
            }

    def keys(self) -> tuple[str, ...]:
        with self._condition:
            return tuple(sorted(self._entries))

    def close(self) -> tuple[str, ...]:
        with self._condition:
            if self._closed:
                return ()
            self._closed = True
            self._condition.notify_all()
        if self._reaper is not None:
            self._reaper.join(timeout=2)
        return self.evict_idle(force=True)

    def _return(self, key: str) -> None:
        with self._lifecycle_lock:
            with self._condition:
                entry = self._entries.get(key)
                if entry is None:
                    return
                if entry.users < 1:
                    raise RuntimeError("resident backend lease count underflow")
                entry.users -= 1
                entry.last_used = self._clock()
                if self._closed and entry.users == 0:
                    self._entries.pop(key)
                    release_backend(entry.value)
                while len(self._entries) > self.max_entries:
                    if not self._evict_one_lru_locked():
                        break
                self._condition.notify_all()

    def _make_room_locked(self) -> None:
        while len(self._entries) >= self.max_entries:
            if not self._evict_one_lru_locked():
                # Active jobs may temporarily exceed the bound; every lease return
                # trims back to the configured maximum.
                return

    def _evict_one_lru_locked(self) -> bool:
        idle = [
            (entry.last_used, key, entry)
            for key, entry in self._entries.items()
            if entry.users == 0
        ]
        if not idle:
            return False
        _last_used, key, entry = min(idle)
        self._entries.pop(key)
        release_backend(entry.value)
        return True

    def _reaper_loop(self) -> None:
        while True:
            with self._condition:
                if self._closed:
                    return
                idle_deadlines = [
                    entry.last_used + self.idle_seconds
                    for entry in self._entries.values()
                    if entry.users == 0
                ]
                if not idle_deadlines:
                    self._condition.wait()
                    continue
                timeout = max(0.0, min(idle_deadlines) - self._clock())
                if timeout > 0:
                    self._condition.wait(timeout)
                    continue
            self.evict_idle()
