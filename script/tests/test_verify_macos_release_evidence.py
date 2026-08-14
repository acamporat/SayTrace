from __future__ import annotations

import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from datetime import UTC, datetime, timedelta
from pathlib import Path

SCRIPT_PATH = Path(__file__).parents[1] / "verify_macos_release_evidence.py"
sys.path.insert(0, str(SCRIPT_PATH.parent))
SPEC = importlib.util.spec_from_file_location(
    "verify_macos_release_evidence", SCRIPT_PATH
)
assert SPEC is not None and SPEC.loader is not None
evidence = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evidence)


class MacOSReleaseEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.now = datetime(2026, 8, 13, 20, 0, tzinfo=UTC)
        self.installer = self.root / "SayTrace-0.3.0-macos-arm64.dmg"
        self.installer.write_bytes(b"signed and notarized installer")
        self.payload = {
            "schema_version": 2,
            "product": "SayTrace macOS physical acceptance",
            "version": "0.3.0",
            "source_revision": "a" * 40,
            "installer": {
                "name": self.installer.name,
                "size": self.installer.stat().st_size,
                "sha256": hashlib.sha256(self.installer.read_bytes()).hexdigest(),
            },
            "performed_at_utc": "2026-08-13T19:00:00Z",
            "hardware": {
                "architecture": "arm64",
                "model_identifier": "Mac16,5",
                "macos_version": "15.6.1",
            },
            "checks": {name: "passed" for name in evidence.REQUIRED_CHECKS},
            "operator_confirmation": True,
        }

    def _write(self) -> Path:
        path = self.root / "acceptance.json"
        path.write_text(json.dumps(self.payload), encoding="utf-8")
        return path

    def _validate(self) -> dict[str, object]:
        return evidence.validate_evidence(
            self._write(),
            version="0.3.0",
            source_revision="a" * 40,
            installer=self.installer,
            now=self.now,
        )

    def test_accepts_complete_current_evidence(self) -> None:
        self.assertEqual(self._validate()["operator_confirmation"], True)

    def test_rejects_release_identity_mismatch(self) -> None:
        with self.assertRaisesRegex(ValueError, "source_revision"):
            evidence.validate_evidence(
                self._write(),
                version="0.3.0",
                source_revision="b" * 40,
                installer=self.installer,
                now=self.now,
            )

    def test_rejects_missing_or_failed_check(self) -> None:
        missing = evidence.REQUIRED_CHECKS[0]
        del self.payload["checks"][missing]
        with self.assertRaisesRegex(ValueError, "inventory"):
            self._validate()
        self.payload["checks"][missing] = "failed"
        with self.assertRaisesRegex(ValueError, "have not passed"):
            self._validate()

    def test_rejects_stale_or_future_timestamp(self) -> None:
        self.payload["performed_at_utc"] = (
            (self.now - timedelta(days=31)).isoformat().replace("+00:00", "Z")
        )
        with self.assertRaisesRegex(ValueError, "older than"):
            self._validate()
        self.payload["performed_at_utc"] = (
            (self.now + timedelta(minutes=6)).isoformat().replace("+00:00", "Z")
        )
        with self.assertRaisesRegex(ValueError, "future"):
            self._validate()

    def test_rejects_symlinked_evidence(self) -> None:
        target = self._write()
        link = self.root / "link.json"
        link.symlink_to(target)
        with self.assertRaisesRegex(ValueError, "ordinary"):
            evidence.validate_evidence(
                link,
                version="0.3.0",
                source_revision="a" * 40,
                installer=self.installer,
                now=self.now,
            )

    def test_rejects_different_installer_bytes(self) -> None:
        self.installer.write_bytes(b"different installer")
        with self.assertRaisesRegex(ValueError, "exact release installer"):
            self._validate()

    def test_rejects_generated_hardware_placeholder(self) -> None:
        self.payload["hardware"]["model_identifier"] = (
            "REPLACE_WITH_MAC_MODEL_IDENTIFIER"
        )
        with self.assertRaisesRegex(ValueError, "real Mac model identifier"):
            self._validate()


if __name__ == "__main__":
    unittest.main()
