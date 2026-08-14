from __future__ import annotations

import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

SCRIPT_ROOT = Path(__file__).parents[1]
VERIFIER = SCRIPT_ROOT / "verify_packaged_worker.py"
FFMPEG_FIXTURE = Path("/usr/bin/true")


FAKE_WORKER_PREAMBLE = r"""#!/usr/bin/env python3
import json
import os
import struct
import sys
import time

HEADER = struct.Struct(">4sBBHQ")


def read_exact(length):
    chunks = []
    remaining = length
    while remaining:
        chunk = sys.stdin.buffer.read(remaining)
        if not chunk:
            raise EOFError
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_message():
    magic, major, kind, flags, length = HEADER.unpack(read_exact(HEADER.size))
    assert (magic, major, kind, flags) == (b"LTW1", 1, 1, 0)
    return json.loads(read_exact(length))


def write_message(message):
    payload = json.dumps(message, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(HEADER.pack(b"LTW1", 1, 1, 0, len(payload)))
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()


def write_hello():
    write_message(
        {
            "protocol_version": "1.0",
            "type": "event",
            "sequence": 0,
            "event": "hello",
            "payload": {"pipeline_version": "test-pipeline"},
        }
    )
"""


class PackagedWorkerVerifierTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def write_worker(self, body: str) -> Path:
        worker = self.root / "fake-worker"
        worker.write_text(
            textwrap.dedent(FAKE_WORKER_PREAMBLE + "\n" + body),
            encoding="utf-8",
        )
        worker.chmod(0o755)
        return worker

    def invoke(self, worker: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(VERIFIER),
                "--worker",
                str(worker),
                "--ffmpeg",
                str(FFMPEG_FIXTURE),
                "--timeout-seconds",
                "2",
            ],
            check=False,
            capture_output=True,
            text=True,
            timeout=20,
        )

    def test_large_stderr_does_not_block_valid_protocol(self) -> None:
        worker = self.write_worker(
            r"""
sys.stderr.write("x" * (256 * 1024 + 1))
sys.stderr.flush()
write_hello()

ping = read_message()
write_message(
    {
        "protocol_version": "1.0",
        "type": "response",
        "request_id": ping["request_id"],
        "ok": True,
        "result": {"pong": True},
    }
)

shutdown = read_message()
write_message(
    {
        "protocol_version": "1.0",
        "type": "response",
        "request_id": shutdown["request_id"],
        "ok": True,
        "result": {"shutting_down": True},
    }
)
"""
        )

        result = self.invoke(worker)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        self.assertIn(
            "Packaged worker hello, ping, and shutdown passed.", result.stdout
        )

    def test_eof_reports_natural_exit_and_original_cause(self) -> None:
        worker = self.write_worker(
            r"""
write_hello()
os.close(sys.stdout.fileno())
time.sleep(0.2)
os._exit(7)
"""
        )

        result = self.invoke(worker)
        diagnostic = result.stdout + result.stderr

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("worker exited on its own with status 7", diagnostic)
        self.assertRegex(diagnostic, r"(?:EOFError|BrokenPipeError):")
        self.assertTrue(
            "Packaged worker closed its protocol output early" in diagnostic
            or "Broken pipe" in diagnostic
        )


if __name__ == "__main__":
    unittest.main()
