from __future__ import annotations

import os
import socket

import pytest

from local_transcript_worker.environment import (
    OFFLINE_ENVIRONMENT,
    enforce_offline_environment,
    restore_socket_for_tests,
)
from local_transcript_worker.errors import ErrorCode, WorkerError


def test_offline_mode_sets_library_flags_and_blocks_sockets() -> None:
    enforce_offline_environment()
    try:
        assert all(os.environ[key] == value for key, value in OFFLINE_ENVIRONMENT.items())
        connection = socket.socket()
        try:
            with pytest.raises(WorkerError) as caught:
                connection.connect(("example.com", 443))
        finally:
            connection.close()
        assert caught.value.code is ErrorCode.OFFLINE_NETWORK_BLOCKED
    finally:
        restore_socket_for_tests()
