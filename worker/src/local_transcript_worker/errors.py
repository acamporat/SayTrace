"""Stable worker error types.

Only error codes are part of the host/worker compatibility contract. Human-readable
messages may improve between releases.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from typing import Any


class ErrorCode(StrEnum):
    BAD_FRAME = "BAD_FRAME"
    BAD_REQUEST = "BAD_REQUEST"
    CANCELLED = "CANCELLED"
    GPU_OUT_OF_MEMORY = "GPU_OUT_OF_MEMORY"
    HASH_MISMATCH = "HASH_MISMATCH"
    INTERNAL = "INTERNAL"
    INVALID_PATH = "INVALID_PATH"
    MODEL_ACCESS_DENIED = "MODEL_ACCESS_DENIED"
    MODEL_DOWNLOAD_FAILED = "MODEL_DOWNLOAD_FAILED"
    MODEL_INSTALL_DISABLED = "MODEL_INSTALL_DISABLED"
    MODEL_MANIFEST_STALE = "MODEL_MANIFEST_STALE"
    MODEL_MISSING = "MODEL_MISSING"
    MODEL_SETUP_FAILED = "MODEL_SETUP_FAILED"
    OFFLINE_NETWORK_BLOCKED = "OFFLINE_NETWORK_BLOCKED"
    OPTIONAL_DEPENDENCY_MISSING = "OPTIONAL_DEPENDENCY_MISSING"
    PROTOCOL_MISMATCH = "PROTOCOL_MISMATCH"
    UNSUPPORTED_AUDIO = "UNSUPPORTED_AUDIO"
    WORKER_BUSY = "WORKER_BUSY"


@dataclass(slots=True)
class WorkerError(Exception):
    code: ErrorCode
    message: str
    details: dict[str, Any] | None = None
    retryable: bool = False

    def as_dict(self) -> dict[str, Any]:
        result: dict[str, Any] = {
            "code": self.code.value,
            "message": self.message,
            "retryable": self.retryable,
        }
        if self.details:
            result["details"] = self.details
        return result


def missing_dependency(package: str, feature: str) -> WorkerError:
    return WorkerError(
        ErrorCode.OPTIONAL_DEPENDENCY_MISSING,
        f"{feature} requires the packaged ML runtime component '{package}'.",
        {"package": package, "feature": feature},
    )
