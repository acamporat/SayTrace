"""Offline execution and canonical-path enforcement."""

from __future__ import annotations

import os
import socket
from pathlib import Path
from typing import Any

from .errors import ErrorCode, WorkerError

OFFLINE_ENVIRONMENT = {
    "DO_NOT_TRACK": "1",
    "HF_DATASETS_OFFLINE": "1",
    "HF_HUB_DISABLE_TELEMETRY": "1",
    "HF_HUB_OFFLINE": "1",
    "PYANNOTE_METRICS_ENABLED": "0",
    "TRANSFORMERS_OFFLINE": "1",
}

_ORIGINAL_SOCKET = socket.socket
_ORIGINAL_GETADDRINFO = socket.getaddrinfo
_GUARD_INSTALLED = False


class _OfflineSocket(_ORIGINAL_SOCKET):
    def connect(self, address: Any) -> None:
        raise WorkerError(
            ErrorCode.OFFLINE_NETWORK_BLOCKED,
            "Network access is disabled in the inference worker.",
            {"address": repr(address)},
        )

    def connect_ex(self, address: Any) -> int:
        raise WorkerError(
            ErrorCode.OFFLINE_NETWORK_BLOCKED,
            "Network access is disabled in the inference worker.",
            {"address": repr(address)},
        )


def _blocked_getaddrinfo(*args: Any, **kwargs: Any) -> Any:
    del kwargs
    host = args[0] if args else None
    raise WorkerError(
        ErrorCode.OFFLINE_NETWORK_BLOCKED,
        "DNS resolution is disabled in the inference worker.",
        {"host": repr(host)},
    )


def enforce_offline_environment(*, install_socket_guard: bool = True) -> None:
    """Disable library telemetry/downloads and optionally block all IP sockets."""

    global _GUARD_INSTALLED
    for key, value in OFFLINE_ENVIRONMENT.items():
        os.environ[key] = value
    if install_socket_guard and not _GUARD_INSTALLED:
        socket.socket = _OfflineSocket  # type: ignore[misc]
        socket.getaddrinfo = _blocked_getaddrinfo
        _GUARD_INSTALLED = True


def restore_socket_for_tests() -> None:
    """Undo process-global guarding; intended only for unit-test isolation."""

    global _GUARD_INSTALLED
    socket.socket = _ORIGINAL_SOCKET  # type: ignore[misc]
    socket.getaddrinfo = _ORIGINAL_GETADDRINFO
    _GUARD_INSTALLED = False


class ApprovedPaths:
    """Resolve host-provided paths while preventing traversal and symlink escape."""

    def __init__(self, roots: list[Path]) -> None:
        if not roots:
            raise WorkerError(ErrorCode.BAD_REQUEST, "At least one approved root is required.")
        self._roots = tuple(root.resolve(strict=True) for root in roots)

    @property
    def roots(self) -> tuple[Path, ...]:
        return self._roots

    def resolve_existing(self, raw_path: str, *, kind: str = "file") -> Path:
        path = Path(raw_path).resolve(strict=True)
        if not self._contains(path):
            raise WorkerError(ErrorCode.INVALID_PATH, "Path is outside approved roots.")
        if kind == "file" and not path.is_file():
            raise WorkerError(ErrorCode.INVALID_PATH, "Expected an existing file.")
        if kind == "directory" and not path.is_dir():
            raise WorkerError(ErrorCode.INVALID_PATH, "Expected an existing directory.")
        return path

    def resolve_output(self, raw_path: str) -> Path:
        path = Path(raw_path).resolve(strict=False)
        parent = path.parent.resolve(strict=True)
        if not self._contains(parent):
            raise WorkerError(ErrorCode.INVALID_PATH, "Output path is outside approved roots.")
        return path

    def _contains(self, path: Path) -> bool:
        return any(path == root or root in path.parents for root in self._roots)
