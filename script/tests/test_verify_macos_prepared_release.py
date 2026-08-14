from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPT_ROOT = Path(__file__).parents[1]
sys.path.insert(0, str(SCRIPT_ROOT))


def load_script(name: str, filename: str):
    specification = importlib.util.spec_from_file_location(name, SCRIPT_ROOT / filename)
    assert specification is not None and specification.loader is not None
    module = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(module)
    return module


generator = load_script(
    "prepared_generator_for_verifier", "generate_macos_prepared_release.py"
)
verifier = load_script(
    "verify_macos_prepared_release", "verify_macos_prepared_release.py"
)

VERSION = "0.3.0"
REVISION = "a" * 40
FFMPEG_VERSION = "8.1.2"
APP_ID = "11111111-1111-4111-8111-111111111111"
DMG_ID = "22222222-2222-4222-8222-222222222222"


class PreparedReleaseVerifierTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / "prepared"
        self.state = self.root / ".release-state"
        self.state.mkdir(parents=True)
        self.dmg = self.root / f"SayTrace-{VERSION}-macos-arm64.dmg"
        self.app_zip = self.state / f"SayTrace-{VERSION}-macos-arm64.zip"
        self.app_result = self.state / "app-notarization.json"
        self.dmg_result = self.state / "dmg-notarization.json"
        self.source = (
            self.root / f"SayTrace-{VERSION}-ffmpeg-{FFMPEG_VERSION}-source.tar.xz"
        )
        self.source_signature = self.source.with_suffix(self.source.suffix + ".asc")
        self.notes = self.root / "RELEASE_NOTES.md"
        self.runtime = self.base / "runtime-manifest.json"
        for path in (
            self.dmg,
            self.app_zip,
            self.source,
            self.source_signature,
            self.notes,
        ):
            path.write_bytes(path.name.encode())
        self.app_result.write_text(
            json.dumps({"id": APP_ID, "name": self.app_zip.name, "status": "Accepted"}),
            encoding="utf-8",
        )
        self.dmg_result.write_text(
            json.dumps({"id": DMG_ID, "name": self.dmg.name, "status": "Accepted"}),
            encoding="utf-8",
        )
        self.runtime.write_text(
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
        arguments = [
            "generate_macos_prepared_release.py",
            "--version",
            VERSION,
            "--source-revision",
            REVISION,
            "--dmg",
            str(self.dmg),
            "--app-notary-payload",
            str(self.app_zip),
            "--app-notary-result",
            str(self.app_result),
            "--dmg-notary-result",
            str(self.dmg_result),
            "--runtime-manifest",
            str(self.runtime),
            "--ffmpeg-source",
            str(self.source),
            "--ffmpeg-source-signature",
            str(self.source_signature),
            "--release-notes",
            str(self.notes),
            "--output",
            str(self.root / "prepared-release.json"),
            "--acceptance-template-output",
            str(self.root / "physical-acceptance-template.json"),
        ]
        with mock.patch.object(sys, "argv", arguments):
            generator.main()

    def validate(self):
        return verifier.validate_prepared_root(
            self.root,
            version=VERSION,
            source_revision=REVISION,
            ffmpeg_version=FFMPEG_VERSION,
            runtime_manifest=self.runtime,
        )

    def test_accepts_exact_prepared_inventory(self) -> None:
        self.assertEqual(self.validate()["distribution"]["notarized"], True)

    def test_rejects_tampered_asset_and_extra_inventory(self) -> None:
        self.dmg.write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "asset digest mismatch"):
            self.validate()
        self.dmg.write_bytes(self.dmg.name.encode())
        (self.root / "unexpected").write_text("extra", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "top-level inventory"):
            self.validate()

    def test_rejects_mismatched_notary_id_and_status(self) -> None:
        manifest_path = self.root / "prepared-release.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["distribution"]["app_notarization_id"] = (
            "33333333-3333-4333-8333-333333333333"
        )
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "app notarization ID"):
            self.validate()

        manifest["distribution"]["app_notarization_id"] = APP_ID
        result = json.loads(self.app_result.read_text(encoding="utf-8"))
        result["status"] = "Invalid"
        self.app_result.write_text(json.dumps(result), encoding="utf-8")
        manifest["assets"]["app_notary_result"] = verifier.file_record(self.app_result)
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "not accepted"):
            self.validate()

    def test_rejects_symlinked_template_and_runtime_manifest(self) -> None:
        template = self.root / "physical-acceptance-template.json"
        outside = self.base / "outside-template.json"
        outside.write_bytes(template.read_bytes())
        template.unlink()
        template.symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "template must be an ordinary"):
            self.validate()

        template.unlink()
        template.write_bytes(outside.read_bytes())
        runtime_link = self.base / "runtime-link.json"
        runtime_link.symlink_to(self.runtime)
        with self.assertRaisesRegex(ValueError, "runtime manifest must be an ordinary"):
            verifier.validate_prepared_root(
                self.root,
                version=VERSION,
                source_revision=REVISION,
                ffmpeg_version=FFMPEG_VERSION,
                runtime_manifest=runtime_link,
            )


class ReleaseShellContractTests(unittest.TestCase):
    def test_expected_apple_team_is_pinned_at_every_signing_boundary(self) -> None:
        sign_source = (SCRIPT_ROOT / "sign_macos_runtime.sh").read_text(
            encoding="utf-8"
        )
        build_source = (SCRIPT_ROOT / "build_macos_release.sh").read_text(
            encoding="utf-8"
        )
        finalize_source = (SCRIPT_ROOT / "finalize_macos_release.sh").read_text(
            encoding="utf-8"
        )

        for source in (sign_source, build_source, finalize_source):
            self.assertIn('EXPECTED_TEAM_ID="ZQF63BSBBN"', source)

        self.assertEqual(sign_source.count('verify_expected_team "$candidate"'), 2)
        self.assertIn('verify_expected_team "$APP_BUNDLE"', build_source)
        self.assertIn('verify_expected_team "$DMG"', build_source)
        self.assertIn('verify_macos_macho.py" --path "$APP_BINARY"', build_source)
        self.assertIn('[[ "$APP_TEAM" == "$EXPECTED_TEAM_ID" ]]', finalize_source)
        self.assertIn('[[ "$DMG_TEAM" == "$EXPECTED_TEAM_ID" ]]', finalize_source)

    def test_finalizer_is_validation_only_and_uses_mounted_app(self) -> None:
        source = (SCRIPT_ROOT / "finalize_macos_release.sh").read_text(encoding="utf-8")
        for forbidden in (
            "codesign --force",
            "stapler staple",
            "notarytool submit",
            "build_worker_macos.sh",
            "build_macos_release.sh",
        ):
            with self.subTest(forbidden=forbidden):
                self.assertNotIn(forbidden, source)
        self.assertIn('--installer "$DMG"', source)
        self.assertIn('--app "$MOUNTED_APP"', source)
        self.assertIn('verify_macos_macho.py" --path "$APP_BINARY"', source)
        self.assertIn("-readonly -nobrowse -noautoopen", source.replace("\\\n", " "))
        self.assertIn("os.rename(sys.argv[1], sys.argv[2])", source)

    def test_prepare_verifies_then_atomically_renames(self) -> None:
        source = (SCRIPT_ROOT / "build_macos_release.sh").read_text(encoding="utf-8")
        verification = source.index("verify_macos_prepared_release.py")
        rename = source.index("os.rename(sys.argv[1], sys.argv[2])")
        self.assertLess(verification, rename)
        self.assertIn(".v$VERSION-prepare.XXXXXX", source)

    def test_release_outputs_and_node_install_are_fail_closed(self) -> None:
        build_source = (SCRIPT_ROOT / "build_macos_release.sh").read_text(
            encoding="utf-8"
        )
        worker_source = (SCRIPT_ROOT / "build_worker_macos.sh").read_text(
            encoding="utf-8"
        )
        ffmpeg_source = (SCRIPT_ROOT / "build_ffmpeg_macos.sh").read_text(
            encoding="utf-8"
        )
        finalize_source = (SCRIPT_ROOT / "finalize_macos_release.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn('EXPECTED_NODE_MAJOR="22"', build_source)
        self.assertIn('EXPECTED_NPM_VERSION="10.9.8"', build_source)
        self.assertIn("npm ci \\", build_source)
        self.assertIn("--ignore-scripts=false", build_source)
        self.assertIn('VITE_DIST_ROOT="$REPOSITORY_BUILD_ROOT/', build_source)
        self.assertIn(
            'mktemp -d "$REPOSITORY_BUILD_ROOT/.tauri-macos-target.XXXXXX"',
            build_source,
        )
        self.assertIn(".v$VERSION-candidate.XXXXXX", build_source)
        self.assertNotIn('rm -rf -- "$ARTIFACT_ROOT"', build_source)
        self.assertIn(
            "Worker runtime output became unsafe before promotion", worker_source
        )
        self.assertIn(
            "FFmpeg cache output became unsafe before promotion", ffmpeg_source
        )
        self.assertIn(
            'assert_existing_output_directory "$PREPARED_ROOT"', finalize_source
        )


if __name__ == "__main__":
    unittest.main()
