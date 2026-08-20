from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import platform
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType
from unittest import mock

SCRIPT_ROOT = Path(__file__).parents[1]
sys.path.insert(0, str(SCRIPT_ROOT))
VERSION = "0.3.0"
SOURCE_REVISION = "a" * 40
APP_NOTARY_ID = "11111111-1111-4111-8111-111111111111"
DMG_NOTARY_ID = "22222222-2222-4222-8222-222222222222"


def load_script(name: str, filename: str) -> ModuleType:
    specification = importlib.util.spec_from_file_location(name, SCRIPT_ROOT / filename)
    assert specification is not None and specification.loader is not None
    module = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(module)
    return module


release_manifest = load_script(
    "generate_macos_release_manifest", "generate_macos_release_manifest.py"
)
runtime_verifier = load_script("verify_macos_runtime", "verify_macos_runtime.py")
macho_verifier = load_script("verify_macos_macho", "verify_macos_macho.py")
runtime_manifest = load_script(
    "generate_macos_runtime_manifest", "generate_macos_runtime_manifest.py"
)
runtime_link_materializer = load_script(
    "materialize_macos_runtime_directory_links",
    "materialize_macos_runtime_directory_links.py",
)


def file_record(path: Path, relative: str) -> dict[str, object]:
    return {
        "path": relative,
        "size": path.stat().st_size,
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
    }


class RuntimeFixture:
    def __init__(self, test_case: unittest.TestCase) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        test_case.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.runtime = self.root / "runtime"
        self.runtime.mkdir()
        self.payload: dict[str, bytes] = {
            "local-transcript-worker": b"worker-mach-o",
            "ffmpeg": b"ffmpeg-mach-o",
            "ffprobe": b"ffprobe-mach-o",
            "_internal/library.dylib": b"library-mach-o",
            "resources/data.txt": b"runtime data",
        }
        for relative, content in self.payload.items():
            path = self.runtime / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
            if relative in runtime_verifier.REQUIRED_ENTRYPOINTS:
                path.chmod(0o755)
        self.manifest_path = self.runtime / "runtime-manifest.json"
        self.write_manifest()

    def manifest(self, **overrides: object) -> dict[str, object]:
        manifest: dict[str, object] = {
            "schema_version": 1,
            "product": "SayTrace Runtime",
            "app_identifier": "com.localtranscript.desktop",
            "runtime_version": VERSION,
            "variant": "apple-mlx",
            "architecture": "arm64",
            "minimum_system_version": "15.0",
            "source_revision": SOURCE_REVISION,
            "component_codesigned": True,
            "runtime_validation": {
                "worker_handshake": "passed",
                "model_inference": "passed",
            },
            "payload": [
                file_record(self.runtime / relative, relative)
                for relative in sorted(self.payload)
            ],
        }
        manifest.update(overrides)
        return manifest

    def write_manifest(self, **overrides: object) -> dict[str, object]:
        manifest = self.manifest(**overrides)
        self.manifest_path.write_text(
            json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
        )
        return manifest


class MacosReleaseManifestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = RuntimeFixture(self)
        self.dmg = self.fixture.root / "SayTrace.dmg"
        self.app = self.fixture.root / "SayTrace.app"
        self.app_zip = self.fixture.root / "SayTrace-0.3.0-macos-arm64.zip"
        self.app_notary_result = self.fixture.root / "app-notarization.json"
        self.dmg_notary_result = self.fixture.root / "dmg-notarization.json"
        self.acceptance = self.fixture.root / "physical-acceptance.json"
        self.ffmpeg_source = self.fixture.root / "ffmpeg-source.tar.xz"
        self.ffmpeg_signature = self.fixture.root / "ffmpeg-source.tar.xz.asc"
        self.output = self.fixture.root / "release-manifest.json"
        self.dmg.write_bytes(b"signed dmg")
        self.app.mkdir()
        self.app_zip.write_bytes(b"notarized app zip")
        self.app_notary_result.write_text(
            json.dumps(
                {
                    "id": APP_NOTARY_ID,
                    "name": self.app_zip.name,
                    "status": "Accepted",
                }
            ),
            encoding="utf-8",
        )
        self.dmg_notary_result.write_text(
            json.dumps(
                {
                    "id": DMG_NOTARY_ID,
                    "name": self.dmg.name,
                    "status": "Accepted",
                }
            ),
            encoding="utf-8",
        )
        self.acceptance.write_text(
            json.dumps(
                {
                    "version": VERSION,
                    "source_revision": SOURCE_REVISION,
                    "installer": {
                        "name": self.dmg.name,
                        "size": self.dmg.stat().st_size,
                        "sha256": hashlib.sha256(self.dmg.read_bytes()).hexdigest(),
                    },
                    "performed_at_utc": "2026-08-13T19:00:00Z",
                    "hardware": {
                        "architecture": "arm64",
                        "macos_version": "15.6",
                        "model_identifier": "Mac16,5",
                    },
                    "schema_version": 3,
                    "product": "SayTrace macOS physical acceptance",
                    "checks": {
                        name: "passed"
                        for name in release_manifest.REQUIRED_ACCEPTANCE_CHECKS
                    },
                    "operator_confirmation": True,
                }
            ),
            encoding="utf-8",
        )
        self.ffmpeg_source.write_bytes(b"verified ffmpeg source")
        self.ffmpeg_signature.write_bytes(b"ffmpeg source signature")

    def arguments(
        self,
        *,
        official: bool = False,
        include_official_inputs: bool = False,
    ) -> list[str]:
        arguments = [
            "generate_macos_release_manifest.py",
            "--version",
            VERSION,
            "--source-revision",
            SOURCE_REVISION,
            "--dmg",
            str(self.dmg),
            "--runtime-manifest",
            str(self.fixture.manifest_path),
            "--ffmpeg-source",
            str(self.ffmpeg_source),
            "--ffmpeg-source-signature",
            str(self.ffmpeg_signature),
            "--output",
            str(self.output),
        ]
        if official:
            arguments.append("--official")
        if include_official_inputs:
            arguments.extend(
                (
                    "--app",
                    str(self.app),
                    "--app-notary-result",
                    str(self.app_notary_result),
                    "--app-notary-payload",
                    str(self.app_zip),
                    "--dmg-notary-result",
                    str(self.dmg_notary_result),
                    "--acceptance-evidence",
                    str(self.acceptance),
                )
            )
        return arguments

    def invoke(self, arguments: list[str]) -> int:
        with mock.patch.object(sys, "argv", arguments):
            return release_manifest.main()

    def test_official_manifest_requires_all_trust_evidence(self) -> None:
        complete = self.arguments(official=True, include_official_inputs=True)
        required_options = (
            "--app",
            "--app-notary-result",
            "--app-notary-payload",
            "--dmg-notary-result",
            "--acceptance-evidence",
        )
        for missing_option in required_options:
            with self.subTest(missing_option=missing_option):
                incomplete = list(complete)
                index = incomplete.index(missing_option)
                del incomplete[index : index + 2]
                with (
                    contextlib.redirect_stderr(io.StringIO()),
                    self.assertRaises(SystemExit) as raised,
                ):
                    self.invoke(incomplete)
                self.assertEqual(raised.exception.code, 2)
                self.assertFalse(self.output.exists())

    def test_candidate_manifest_rejects_official_trust_evidence(self) -> None:
        with (
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit) as raised,
        ):
            self.invoke(self.arguments(include_official_inputs=True))
        self.assertEqual(raised.exception.code, 2)
        self.assertFalse(self.output.exists())

    def test_runtime_identity_mismatches_are_rejected(self) -> None:
        mismatches = (
            ("runtime_version", "9.9.9", "version"),
            ("source_revision", "b" * 40, "source revision"),
            ("component_codesigned", False, "signed components"),
        )
        for field, value, message in mismatches:
            with self.subTest(field=field):
                self.fixture.write_manifest(**{field: value})
                with self.assertRaisesRegex(ValueError, message):
                    self.invoke(self.arguments())
                self.assertFalse(self.output.exists())

    def test_official_manifest_requires_model_inference(self) -> None:
        self.fixture.write_manifest(
            runtime_validation={
                "worker_handshake": "passed",
                "model_inference": "not_run",
            }
        )
        with self.assertRaisesRegex(ValueError, "validation has not passed"):
            self.invoke(self.arguments(official=True, include_official_inputs=True))
        self.assertFalse(self.output.exists())

    def test_official_manifest_requires_exact_acceptance_inventory(self) -> None:
        evidence = json.loads(self.acceptance.read_text(encoding="utf-8"))
        del evidence["checks"][release_manifest.REQUIRED_ACCEPTANCE_CHECKS[0]]
        self.acceptance.write_text(json.dumps(evidence), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "check inventory is not exact"):
            self.invoke(self.arguments(official=True, include_official_inputs=True))
        self.assertFalse(self.output.exists())

    def test_official_manifest_rejects_placeholder_hardware_and_stale_time(
        self,
    ) -> None:
        evidence = json.loads(self.acceptance.read_text(encoding="utf-8"))
        evidence["hardware"]["model_identifier"] = "REPLACE_WITH_MAC_MODEL_IDENTIFIER"
        self.acceptance.write_text(json.dumps(evidence), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "real Mac model identifier"):
            self.invoke(self.arguments(official=True, include_official_inputs=True))

        evidence["hardware"]["model_identifier"] = "Mac16,5"
        evidence["performed_at_utc"] = "2020-01-01T00:00:00Z"
        self.acceptance.write_text(json.dumps(evidence), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "older than 30 days"):
            self.invoke(self.arguments(official=True, include_official_inputs=True))

    def test_successful_official_manifest_records_artifacts(self) -> None:
        def fake_run(
            command: list[str], **_: object
        ) -> subprocess.CompletedProcess[str]:
            details = (
                "Authority=Developer ID Application: Example (TEAMID)\n"
                "TeamIdentifier=TEAMID\n"
                if command[:3]
                == [
                    "/usr/bin/codesign",
                    "--display",
                    "--verbose=4",
                ]
                else ""
            )
            return subprocess.CompletedProcess(command, 0, stdout="", stderr=details)

        with mock.patch.object(
            release_manifest.subprocess, "run", side_effect=fake_run
        ):
            result = self.invoke(
                self.arguments(official=True, include_official_inputs=True)
            )

        self.assertEqual(result, 0)
        generated = json.loads(self.output.read_text(encoding="utf-8"))
        self.assertEqual(generated["version"], VERSION)
        self.assertEqual(generated["tag"], f"v{VERSION}")
        self.assertEqual(generated["source_revision"], SOURCE_REVISION)
        self.assertEqual(
            generated["distribution"],
            {
                "official": True,
                "developer_id_signed": True,
                "notarized": True,
                "stapled": True,
                "app_notarization_id": APP_NOTARY_ID,
                "dmg_notarization_id": DMG_NOTARY_ID,
            },
        )
        self.assertEqual(
            generated["runtime"]["manifest_sha256"],
            hashlib.sha256(self.fixture.manifest_path.read_bytes()).hexdigest(),
        )
        self.assertEqual(generated["runtime"]["worker_handshake"], "passed")
        self.assertEqual(generated["runtime"]["model_inference"], "passed")
        self.assertEqual(
            generated["physical_acceptance"]["evidence"]["sha256"],
            hashlib.sha256(self.acceptance.read_bytes()).hexdigest(),
        )
        for key, source in (
            ("installer", self.dmg),
            ("ffmpeg_source", self.ffmpeg_source),
            ("ffmpeg_source_signature", self.ffmpeg_signature),
        ):
            self.assertEqual(generated["assets"][key]["name"], source.name)
            self.assertEqual(generated["assets"][key]["size"], source.stat().st_size)
            self.assertEqual(
                generated["assets"][key]["sha256"],
                hashlib.sha256(source.read_bytes()).hexdigest(),
            )


class MacosRuntimeVerifierTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = RuntimeFixture(self)

    def arguments(self) -> list[str]:
        return [
            "verify_macos_runtime.py",
            "--runtime",
            str(self.fixture.runtime),
            "--version",
            VERSION,
            "--source-revision",
            SOURCE_REVISION,
        ]

    def invoke(self) -> int:
        with mock.patch.object(sys, "argv", self.arguments()):
            return runtime_verifier.main()

    def test_runtime_identity_mismatches_are_rejected(self) -> None:
        mismatches = (
            ("runtime_version", "9.9.9"),
            ("source_revision", "b" * 40),
            ("component_codesigned", False),
        )
        for field, value in mismatches:
            with self.subTest(field=field):
                self.fixture.write_manifest(**{field: value})
                with self.assertRaisesRegex(ValueError, repr(field)):
                    self.invoke()

    def test_payload_digest_and_exhaustiveness_are_enforced(self) -> None:
        manifest = self.fixture.write_manifest()
        payload = manifest["payload"]
        assert isinstance(payload, list)
        first = payload[0]
        assert isinstance(first, dict)
        first["sha256"] = "0" * 64
        self.fixture.manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            self.invoke()

        self.fixture.write_manifest()
        unlisted = self.fixture.runtime / "unlisted.bin"
        unlisted.write_bytes(b"not declared")
        with self.assertRaisesRegex(ValueError, "enumeration does not match"):
            self.invoke()

    def test_nested_manifest_name_is_not_excluded_from_payload(self) -> None:
        nested = self.fixture.runtime / "resources/runtime-manifest.json"
        nested.write_text("nested payload", encoding="utf-8")

        with self.assertRaisesRegex(ValueError, "enumeration does not match"):
            self.invoke()

    def test_directory_symlinks_and_unsupported_entries_are_rejected(self) -> None:
        target = self.fixture.runtime / "resources"
        link = self.fixture.runtime / "resources-link"
        link.symlink_to(target, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "links must resolve to files"):
            self.invoke()

        link.unlink()
        fifo = self.fixture.runtime / "unsupported-fifo"
        os.mkfifo(fifo)
        with self.assertRaisesRegex(ValueError, "unsupported filesystem entry"):
            self.invoke()

    def test_unsafe_manifest_paths_are_rejected(self) -> None:
        unsafe = (
            "",
            "/absolute/path",
            "../escape",
            "directory/../escape",
            r"directory\escape",
            "C:escape",
        )
        for raw_path in unsafe:
            with (
                self.subTest(raw_path=raw_path),
                self.assertRaisesRegex(ValueError, "Unsafe runtime manifest path"),
            ):
                runtime_verifier.safe_relative_path(raw_path)

    def test_success_verifies_macho_payloads_without_real_codesign(self) -> None:
        subprocess_calls: list[list[str]] = []

        def fake_run(
            command: list[str], **_: object
        ) -> subprocess.CompletedProcess[object]:
            subprocess_calls.append(command)
            path = Path(command[-1])
            if command[0] == "/usr/bin/file":
                description = (
                    b"Mach-O 64-bit executable arm64\n"
                    if path.name
                    in {*runtime_verifier.REQUIRED_ENTRYPOINTS, "library.dylib"}
                    else b"data with arbitrary byte \xa9\n"
                )
                return subprocess.CompletedProcess(
                    command, 0, stdout=description, stderr=b""
                )
            if command[0] == "/usr/bin/codesign":
                return subprocess.CompletedProcess(command, 0, stdout=b"", stderr=b"")
            if command[0] == "/usr/bin/vtool":
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout=("cmd LC_BUILD_VERSION\n  platform MACOS\n  minos 15.0\n"),
                    stderr="",
                )
            self.fail(f"Unexpected subprocess: {command}")

        with (
            mock.patch.object(runtime_verifier.subprocess, "run", side_effect=fake_run),
            contextlib.redirect_stdout(io.StringIO()) as stdout,
        ):
            result = self.invoke()

        self.assertEqual(result, 0)
        self.assertIn("Verified 5 signed runtime payload files.", stdout.getvalue())
        codesign_calls = [
            call for call in subprocess_calls if call[0] == "/usr/bin/codesign"
        ]
        self.assertEqual(len(codesign_calls), 4)
        self.assertTrue(
            all(call[1:3] == ["--verify", "--strict"] for call in codesign_calls)
        )

    def test_rejects_macho_newer_than_declared_minimum(self) -> None:
        def fake_run(
            command: list[str], **_: object
        ) -> subprocess.CompletedProcess[object]:
            if command[0] == "/usr/bin/file":
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout="Mach-O 64-bit executable arm64\n",
                    stderr="",
                )
            if command[0] == "/usr/bin/vtool":
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout=("cmd LC_BUILD_VERSION\n  platform MACOS\n  minos 26.2\n"),
                    stderr="",
                )
            self.fail(f"Unexpected subprocess: {command}")

        with (
            mock.patch.object(runtime_verifier.subprocess, "run", side_effect=fake_run),
            self.assertRaisesRegex(ValueError, "newer macOS version than 15.0"),
        ):
            self.invoke()

    def test_rejects_non_macos_macho_platform(self) -> None:
        def fake_run(
            command: list[str], **_: object
        ) -> subprocess.CompletedProcess[object]:
            if command[0] == "/usr/bin/file":
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout="Mach-O 64-bit executable arm64\n",
                    stderr="",
                )
            if command[0] == "/usr/bin/vtool":
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout=("cmd LC_BUILD_VERSION\n  platform IOS\n  minos 15.0\n"),
                    stderr="",
                )
            self.fail(f"Unexpected subprocess: {command}")

        with (
            mock.patch.object(runtime_verifier.subprocess, "run", side_effect=fake_run),
            self.assertRaisesRegex(ValueError, "non-macOS platform"),
        ):
            self.invoke()

    def test_accepts_legacy_macos_version_command(self) -> None:
        self.assertEqual(
            runtime_verifier.parse_macos_build_versions(
                "cmd LC_VERSION_MIN_MACOSX\n  version 12.0\n"
            ),
            [(12, 0)],
        )


class MacosExecutableVerifierTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.executable = Path(self.temporary.name) / "SayTrace"
        self.executable.write_bytes(b"test Mach-O")
        self.executable.chmod(0o755)

    def invoke(self) -> int:
        arguments = ["verify_macos_macho.py", "--path", str(self.executable)]
        with mock.patch.object(sys, "argv", arguments):
            return macho_verifier.main()

    def test_verifies_signature_architecture_platform_and_target(self) -> None:
        calls: list[list[str]] = []

        def fake_run(
            command: list[str], **_: object
        ) -> subprocess.CompletedProcess[object]:
            calls.append(command)
            if command[0] == "/usr/bin/file":
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout=b"Mach-O 64-bit executable arm64\n",
                    stderr=b"",
                )
            if command[0] == "/usr/bin/vtool":
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout="cmd LC_BUILD_VERSION\n  platform MACOS\n  minos 15.0\n",
                    stderr="",
                )
            if command[0] == "/usr/bin/codesign":
                return subprocess.CompletedProcess(command, 0, stdout=b"", stderr=b"")
            self.fail(f"Unexpected subprocess: {command}")

        with (
            mock.patch.object(macho_verifier.subprocess, "run", side_effect=fake_run),
            contextlib.redirect_stdout(io.StringIO()) as stdout,
        ):
            self.assertEqual(self.invoke(), 0)

        self.assertEqual(
            [call[0] for call in calls],
            ["/usr/bin/file", "/usr/bin/vtool", "/usr/bin/codesign"],
        )
        self.assertIn("Verified signed Apple Silicon", stdout.getvalue())

    def test_rejects_symlink_and_non_macho_inputs(self) -> None:
        target = self.executable
        link = target.with_name("SayTrace-link")
        link.symlink_to(target)
        self.executable = link
        with self.assertRaisesRegex(ValueError, "ordinary file"):
            self.invoke()

        self.executable = target
        result = subprocess.CompletedProcess(
            ["/usr/bin/file"], 0, stdout=b"ASCII text\n", stderr=b""
        )
        with (
            mock.patch.object(macho_verifier.subprocess, "run", return_value=result),
            self.assertRaisesRegex(ValueError, "not a Mach-O"),
        ):
            self.invoke()


class MacosRuntimeManifestGenerationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.runtime = Path(self.temporary.name) / "runtime"
        self.runtime.mkdir()
        self.manifest = self.runtime / runtime_manifest.RUNTIME_MANIFEST_NAME
        for entrypoint in runtime_manifest.REQUIRED_ENTRYPOINTS:
            path = self.runtime / entrypoint
            path.write_bytes(entrypoint.encode("utf-8"))
            path.chmod(0o755)

    def records(self) -> list[dict[str, object]]:
        return runtime_manifest.payload_records(self.runtime, self.manifest)

    def test_excludes_only_the_root_runtime_manifest(self) -> None:
        self.manifest.write_text("root manifest", encoding="utf-8")
        nested = self.runtime / "resources/runtime-manifest.json"
        nested.parent.mkdir()
        nested.write_text("nested payload", encoding="utf-8")

        paths = {str(record["path"]) for record in self.records()}

        self.assertNotIn("runtime-manifest.json", paths)
        self.assertIn("resources/runtime-manifest.json", paths)

    def test_rejects_directory_symlinks_and_unsupported_entries(self) -> None:
        directory = self.runtime / "data"
        directory.mkdir()
        link = self.runtime / "data-link"
        link.symlink_to(directory, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "links must resolve to files"):
            self.records()

        link.unlink()
        fifo = self.runtime / "unsupported-fifo"
        os.mkfifo(fifo)
        with self.assertRaisesRegex(ValueError, "unsupported filesystem entry"):
            self.records()


class MacosRuntimeDirectoryLinkMaterializationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.runtime = Path(self.temporary.name) / "runtime"
        self.runtime.mkdir()

    def test_materializes_internal_directory_links_and_preserves_file_links(
        self,
    ) -> None:
        target = self.runtime / "framework/Versions/3.13"
        target.mkdir(parents=True)
        payload = target / "Python"
        payload.write_bytes(b"python runtime")
        directory_link = self.runtime / "framework/Versions/Current"
        directory_link.symlink_to("3.13", target_is_directory=True)
        file_link = self.runtime / "Python"
        file_link.symlink_to("framework/Versions/3.13/Python")

        count = runtime_link_materializer.materialize_directory_links(self.runtime)

        self.assertEqual(count, 1)
        self.assertTrue(directory_link.is_dir())
        self.assertFalse(directory_link.is_symlink())
        self.assertEqual((directory_link / "Python").read_bytes(), b"python runtime")
        self.assertTrue(file_link.is_symlink())
        self.assertEqual(file_link.read_bytes(), b"python runtime")

    def test_rejects_broken_and_escaping_links(self) -> None:
        outside = Path(self.temporary.name) / "outside"
        outside.mkdir()
        escaping = self.runtime / "escaping"
        escaping.symlink_to("../outside", target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "escapes the staging root"):
            runtime_link_materializer.materialize_directory_links(self.runtime)

        escaping.unlink()
        broken = self.runtime / "broken"
        broken.symlink_to("missing")
        with self.assertRaisesRegex(ValueError, "link is invalid"):
            runtime_link_materializer.materialize_directory_links(self.runtime)

    def test_rejects_self_containing_directory_link_before_copy(self) -> None:
        directory = self.runtime / "a"
        directory.mkdir()
        link = directory / "loop"
        link.symlink_to(".", target_is_directory=True)

        with (
            mock.patch.object(runtime_link_materializer.shutil, "copytree") as copytree,
            self.assertRaisesRegex(ValueError, "target contains the link itself"),
        ):
            runtime_link_materializer.materialize_directory_links(self.runtime)

        copytree.assert_not_called()
        self.assertTrue(link.is_symlink())

    def test_rejects_ancestor_directory_link_before_copy(self) -> None:
        directory = self.runtime / "a/b"
        directory.mkdir(parents=True)
        link = directory / "loop"
        link.symlink_to("../..", target_is_directory=True)

        with (
            mock.patch.object(runtime_link_materializer.shutil, "copytree") as copytree,
            self.assertRaisesRegex(ValueError, "target contains the link itself"),
        ):
            runtime_link_materializer.materialize_directory_links(self.runtime)

        copytree.assert_not_called()
        self.assertTrue(link.is_symlink())


@unittest.skipUnless(
    sys.platform == "darwin" and platform.machine() == "arm64",
    "runtime signing path checks require Apple Silicon macOS",
)
class MacosRuntimeSigningPathTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repository = SCRIPT_ROOT.parent
        cls.approved_parent = cls.repository / "build/macos-runtime"
        cls.approved_parent.mkdir(parents=True, exist_ok=True)
        cls.sign_script = SCRIPT_ROOT / "sign_macos_runtime.sh"

    @staticmethod
    def make_entrypoint(path: Path) -> None:
        path.write_bytes(b"test executable")
        path.chmod(0o755)

    def invoke(self, runtime: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                "/bin/bash",
                str(self.sign_script),
                "--development",
                "--runtime",
                str(runtime),
            ],
            check=False,
            capture_output=True,
            text=True,
        )

    def test_rejects_runtime_outside_approved_build_subtree(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            runtime = Path(temporary)
            for entrypoint in runtime_manifest.REQUIRED_ENTRYPOINTS:
                self.make_entrypoint(runtime / entrypoint)

            result = self.invoke(runtime)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("restricted to", result.stderr)

    def test_rejects_symlink_entrypoint_before_signing_or_execution(self) -> None:
        with (
            tempfile.TemporaryDirectory(dir=self.approved_parent) as runtime_directory,
            tempfile.TemporaryDirectory() as external_directory,
        ):
            runtime = Path(runtime_directory)
            external_worker = Path(external_directory) / "worker"
            self.make_entrypoint(external_worker)
            (runtime / "local-transcript-worker").symlink_to(external_worker)
            self.make_entrypoint(runtime / "ffmpeg")
            self.make_entrypoint(runtime / "ffprobe")

            result = self.invoke(runtime)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must not be a symbolic link", result.stderr)

    def test_rejects_symlink_runtime_root(self) -> None:
        with tempfile.TemporaryDirectory(dir=self.approved_parent) as runtime_directory:
            runtime = Path(runtime_directory)
            for entrypoint in runtime_manifest.REQUIRED_ENTRYPOINTS:
                self.make_entrypoint(runtime / entrypoint)
            link = self.approved_parent / f"{runtime.name}-link"
            link.symlink_to(runtime, target_is_directory=True)
            self.addCleanup(link.unlink, missing_ok=True)

            result = self.invoke(link)

            link.unlink()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("root must not be a symbolic link", result.stderr)


if __name__ == "__main__":
    unittest.main()
