from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).parents[1] / "generate_macos_prepared_release.py"
sys.path.insert(0, str(SCRIPT.parent))
SPEC = importlib.util.spec_from_file_location("generate_macos_prepared_release", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
prepared_release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prepared_release)

VERSION = "0.3.0"
REVISION = "a" * 40


class PreparedReleaseTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.paths = {
            name: self.root / filename
            for name, filename in {
                "dmg": "SayTrace-0.3.0-macos-arm64.dmg",
                "app_zip": "SayTrace-0.3.0-macos-arm64.zip",
                "app_result": "app-notarization.json",
                "dmg_result": "dmg-notarization.json",
                "runtime": "runtime-manifest.json",
                "source": "ffmpeg.tar.xz",
                "source_signature": "ffmpeg.tar.xz.asc",
                "notes": "RELEASE_NOTES.md",
                "output": "prepared-release.json",
                "template": "physical-acceptance.json",
            }.items()
        }
        for key in ("dmg", "app_zip", "source", "source_signature", "notes"):
            self.paths[key].write_bytes(key.encode())
        self.paths["app_result"].write_text(
            json.dumps(
                {
                    "id": "11111111-1111-4111-8111-111111111111",
                    "name": self.paths["app_zip"].name,
                    "status": "Accepted",
                }
            ),
            encoding="utf-8",
        )
        self.paths["dmg_result"].write_text(
            json.dumps(
                {
                    "id": "22222222-2222-4222-8222-222222222222",
                    "name": self.paths["dmg"].name,
                    "status": "Accepted",
                }
            ),
            encoding="utf-8",
        )
        self.paths["runtime"].write_text(
            json.dumps(
                {
                    "runtime_version": VERSION,
                    "source_revision": REVISION,
                    "component_codesigned": True,
                    "runtime_validation": {
                        "worker_handshake": "passed",
                        "model_inference": "passed",
                    },
                }
            ),
            encoding="utf-8",
        )

    def arguments(self) -> list[str]:
        return [
            "generate_macos_prepared_release.py",
            "--version",
            VERSION,
            "--source-revision",
            REVISION,
            "--dmg",
            str(self.paths["dmg"]),
            "--app-notary-payload",
            str(self.paths["app_zip"]),
            "--app-notary-result",
            str(self.paths["app_result"]),
            "--dmg-notary-result",
            str(self.paths["dmg_result"]),
            "--runtime-manifest",
            str(self.paths["runtime"]),
            "--ffmpeg-source",
            str(self.paths["source"]),
            "--ffmpeg-source-signature",
            str(self.paths["source_signature"]),
            "--release-notes",
            str(self.paths["notes"]),
            "--output",
            str(self.paths["output"]),
            "--acceptance-template-output",
            str(self.paths["template"]),
        ]

    def invoke(self) -> int:
        with mock.patch.object(sys, "argv", self.arguments()):
            return prepared_release.main()

    def test_records_exact_installer_and_prefills_acceptance(self) -> None:
        self.assertEqual(self.invoke(), 0)
        prepared = json.loads(self.paths["output"].read_text(encoding="utf-8"))
        acceptance = json.loads(self.paths["template"].read_text(encoding="utf-8"))
        self.assertEqual(
            prepared["state"], "awaiting_exact_installer_physical_acceptance"
        )
        self.assertEqual(acceptance["schema_version"], 2)
        self.assertEqual(acceptance["installer"], prepared["assets"]["installer"])
        self.assertEqual(
            set(acceptance["checks"]), set(prepared_release.REQUIRED_ACCEPTANCE_CHECKS)
        )

    def test_rejects_runtime_without_packaged_inference(self) -> None:
        runtime = json.loads(self.paths["runtime"].read_text(encoding="utf-8"))
        runtime["runtime_validation"]["model_inference"] = "not_run"
        self.paths["runtime"].write_text(json.dumps(runtime), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "validation is incomplete"):
            self.invoke()

    def test_rejects_wrong_notary_artifact_name(self) -> None:
        result = json.loads(self.paths["dmg_result"].read_text(encoding="utf-8"))
        result["name"] = "other.dmg"
        self.paths["dmg_result"].write_text(json.dumps(result), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "wrong artifact name"):
            self.invoke()


if __name__ == "__main__":
    unittest.main()
