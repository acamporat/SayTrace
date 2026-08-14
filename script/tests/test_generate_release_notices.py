from __future__ import annotations

import importlib.util
import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest import mock

SCRIPT_PATH = Path(__file__).parents[1] / "generate_release_notices.py"
SPEC = importlib.util.spec_from_file_location("generate_release_notices", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
notices = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(notices)


class ReleaseNoticeGeneratorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repository = Path(self.temporary.name) / "repository"
        self.repository.mkdir()
        self._write("LICENSE", "Project license\n")
        self._write("NOTICE", "Project notice\n")
        self._write("THIRD_PARTY_NOTICES.md", "Third-party summary\n")
        self._write(
            "src-tauri/Cargo.toml", "[package]\nname='fixture'\nversion='1.0.0'\n"
        )
        self._write(
            "worker/pyproject.toml",
            "[project]\n"
            "name='local-transcript-worker'\n"
            "version='0.1.0'\n"
            "license='Apache-2.0'\n",
        )
        self._write("worker/model-manifest.json", '{"pipeline_version":"windows"}\n')
        self._write(
            "worker/model-manifest.macos.json", '{"pipeline_version":"macos"}\n'
        )

        package_lock = {
            "name": "fixture",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "fixture", "version": "1.0.0"},
                "node_modules/node-fixture": {
                    "name": "node-fixture",
                    "version": "2.0.0",
                    "license": "MIT",
                },
            },
        }
        self._write("package-lock.json", json.dumps(package_lock))
        self._write(
            "node_modules/node-fixture/package.json",
            json.dumps({"name": "node-fixture", "version": "2.0.0", "license": "MIT"}),
        )
        self._write("node_modules/node-fixture/LICENSE", "Node license\n")

        self.rust_dependency = self.repository / "fixtures/rust-fixture"
        self._write(
            "fixtures/rust-fixture/Cargo.toml",
            "[package]\nname='rust-fixture'\nversion='3.0.0'\n",
        )
        self._write("fixtures/rust-fixture/LICENSE-APACHE", "Rust license\n")

        self.python_license = self.repository / "fixtures/python-fixture/LICENSE.txt"
        self._write("fixtures/python-fixture/LICENSE.txt", "Python license\n")

        self.cargo = self.repository / "tools/fake-cargo"
        self._write_executable(
            self.cargo,
            """#!/usr/bin/env python3
import json
import os
from pathlib import Path
root = Path(os.environ["NOTICE_FIXTURE_REPOSITORY"])
print(json.dumps({
    "workspace_members": ["fixture 1.0.0"],
    "resolve": {
        "root": "fixture 1.0.0",
        "nodes": [
            {
                "id": "fixture 1.0.0",
                "deps": [{"pkg": "rust-fixture 3.0.0"}],
            },
            {"id": "rust-fixture 3.0.0", "deps": []},
        ],
    },
    "packages": [
        {
            "id": "fixture 1.0.0",
            "name": "fixture",
            "version": "1.0.0",
            "license": "Apache-2.0",
            "source": None,
            "manifest_path": str(root / "src-tauri/Cargo.toml"),
        },
        {
            "id": "rust-fixture 3.0.0",
            "name": "rust-fixture",
            "version": "3.0.0",
            "license": "Apache-2.0",
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "manifest_path": str(root / "fixtures/rust-fixture/Cargo.toml"),
        },
    ],
}))
""",
        )

        self.python = self.repository / "tools/fake-python"
        self._write_executable(
            self.python,
            """#!/usr/bin/env python3
import json
import os
from pathlib import Path
root = Path(os.environ["NOTICE_FIXTURE_REPOSITORY"])
configured = os.environ.get("NOTICE_FIXTURE_PYTHON_RECORDS")
if configured:
    print(configured)
else:
    print(json.dumps([{
        "name": "python-fixture",
        "version": "4.0.0",
        "license_expression": "BSD-3-Clause",
        "license": None,
        "license_files": [{
            "relative": "LICENSE.txt",
            "path": str(root / "fixtures/python-fixture/LICENSE.txt"),
        }],
    }]))
""",
        )

        self.ffmpeg_prefix = self.repository / "fixtures/ffmpeg"
        self._write_executable(
            self.ffmpeg_prefix / "bin/ffmpeg",
            """#!/bin/sh
printf '%s\n' 'ffmpeg version 8.0.1' \
  'configuration: --prefix=/tmp/ffmpeg --disable-gpl --disable-nonfree --enable-static'
            """,
        )
        self.ffmpeg_source = self.repository / "fixtures/ffmpeg-source"
        self._write("fixtures/ffmpeg-source/LICENSE.md", "FFmpeg licensing summary\n")
        self._write("fixtures/ffmpeg-source/COPYING.LGPLv2.1", "FFmpeg LGPL license\n")
        self._write(
            "fixtures/ffmpeg/share/saytrace-ffmpeg/source-url.txt",
            "https://example.com/ffmpeg/source\n",
        )
        self._write(
            "fixtures/ffmpeg/share/saytrace-ffmpeg/source-sha256.txt",
            f"{'a' * 64}\n",
        )
        self._write(
            "fixtures/ffmpeg/share/saytrace-ffmpeg/source-signing-key-fingerprint.txt",
            f"{'B' * 40}\n",
        )

    def _write(self, relative: str, content: str) -> Path:
        path = self.repository / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path

    def _write_executable(self, path: Path, content: str) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)

    @staticmethod
    def _snapshot(root: Path) -> dict[str, bytes]:
        return {
            path.relative_to(root).as_posix(): path.read_bytes()
            for path in sorted(root.rglob("*"))
            if path.is_file()
        }

    def _run_generate(
        self, output: str, python_records: list[dict[str, object]] | None = None
    ) -> tuple[int, str]:
        environment = {
            "CARGO": str(self.cargo),
            "NOTICE_FIXTURE_REPOSITORY": str(self.repository),
        }
        if python_records is not None:
            environment["NOTICE_FIXTURE_PYTHON_RECORDS"] = json.dumps(python_records)
        stderr = io.StringIO()
        with (
            mock.patch.dict(os.environ, environment, clear=False),
            redirect_stderr(stderr),
        ):
            result = notices.main(
                [
                    "--repository-root",
                    str(self.repository),
                    "--output",
                    output,
                    "--ffmpeg-source",
                    str(self.ffmpeg_source),
                    "--ffmpeg-prefix",
                    str(self.ffmpeg_prefix),
                    "--python",
                    str(self.python),
                ]
            )
        return result, stderr.getvalue()

    def _generate(self, output: str) -> Path:
        result, stderr = self._run_generate(output)
        self.assertEqual(result, 0)
        self.assertEqual(stderr, "")
        return self.repository / output

    def _write_python_override(
        self, name: str, version: str
    ) -> tuple[Path, Path, Path]:
        directory = f"third_party/licenses/python/{name}-{version}"
        license_path = self._write(f"{directory}/LICENSE", "Fixture MIT license\n")
        attribution_path = self._write(
            f"{directory}/SOURCE.md", "Fixture source attribution\n"
        )
        revision = "c" * 40
        manifest = {
            "schema_version": 1,
            "packages": [
                {
                    "attribution_file": attribution_path.relative_to(
                        self.repository
                    ).as_posix(),
                    "attribution_sha256": notices.sha256_file(attribution_path),
                    "distribution_filename": (
                        f"{name.replace('-', '_')}-{version}-py3-none-any.whl"
                    ),
                    "distribution_sha256": "d" * 64,
                    "license": "MIT",
                    "license_file": license_path.relative_to(
                        self.repository
                    ).as_posix(),
                    "license_sha256": notices.sha256_file(license_path),
                    "license_url": (
                        "https://raw.githubusercontent.com/example/project/"
                        f"{revision}/LICENSE"
                    ),
                    "name": name,
                    "package_index_url": (
                        f"https://pypi.org/pypi/{name}/{version}/json"
                    ),
                    "source_revision": revision,
                    "source_url": "https://github.com/example/project",
                    "version": version,
                }
            ],
        }
        manifest_path = self._write(
            notices.PYTHON_LICENSE_OVERRIDES_FILE, json.dumps(manifest)
        )
        return manifest_path, license_path, attribution_path

    def _package_lock(self) -> dict[str, object]:
        return json.loads((self.repository / "package-lock.json").read_text())

    def _replace_package_lock(self, lock: dict[str, object]) -> None:
        self._write("package-lock.json", json.dumps(lock))

    def test_generates_stable_inventory_and_license_tree(self) -> None:
        first = self._generate("build/notices-first")
        second = self._generate("build/notices-second")

        self.assertEqual(self._snapshot(first), self._snapshot(second))
        inventory = json.loads((first / "dependency-inventory.json").read_text())
        self.assertEqual(inventory["dependencies"]["node"][0]["name"], "node-fixture")
        self.assertEqual(inventory["dependencies"]["rust"][0]["name"], "rust-fixture")
        self.assertEqual(
            inventory["dependencies"]["python"][0]["name"], "python-fixture"
        )
        self.assertIn(
            "--prefix=<local-path>", (first / "ffmpeg/configuration.txt").read_text()
        )
        self.assertNotIn(str(self.repository), json.dumps(inventory))
        self.assertTrue(any((first / "licenses/node").rglob("LICENSE")))
        self.assertTrue(any((first / "licenses/rust").rglob("LICENSE-APACHE")))
        self.assertTrue(any((first / "licenses/python").rglob("LICENSE.txt")))

    def test_replaces_only_a_repository_build_subdirectory(self) -> None:
        existing = self.repository / "build/notices"
        existing.mkdir(parents=True)
        (existing / "stale.txt").write_text("stale", encoding="utf-8")

        generated = self._generate("build/notices")

        self.assertFalse((generated / "stale.txt").exists())
        with self.assertRaises(notices.ReleaseNoticeError):
            notices.validate_output_path(self.repository, Path("outside/notices"))
        with self.assertRaises(notices.ReleaseNoticeError):
            notices.validate_output_path(self.repository, Path("build"))

    def test_fails_when_required_locked_node_package_is_missing(self) -> None:
        lock = self._package_lock()
        packages = lock["packages"]
        assert isinstance(packages, dict)
        packages["node_modules/required-missing"] = {
            "version": "1.2.3",
            "license": "MIT",
        }
        self._replace_package_lock(lock)

        result, stderr = self._run_generate("build/notices")

        self.assertEqual(result, 1)
        self.assertIn(
            "Required locked Node package is not installed: required-missing==1.2.3",
            stderr,
        )

    def test_fails_when_installed_node_package_name_mismatches_lock(self) -> None:
        self._write(
            "node_modules/node-fixture/package.json",
            json.dumps(
                {"name": "different-fixture", "version": "2.0.0", "license": "MIT"}
            ),
        )

        result, stderr = self._run_generate("build/notices")

        self.assertEqual(result, 1)
        self.assertIn(
            "Installed Node package name does not match package-lock.json",
            stderr,
        )
        self.assertIn("expected 'node-fixture', found 'different-fixture'", stderr)

    def test_fails_when_installed_node_package_version_mismatches_lock(self) -> None:
        self._write(
            "node_modules/node-fixture/package.json",
            json.dumps({"name": "node-fixture", "version": "2.0.1", "license": "MIT"}),
        )

        result, stderr = self._run_generate("build/notices")

        self.assertEqual(result, 1)
        self.assertIn(
            "Installed Node package version does not match package-lock.json",
            stderr,
        )
        self.assertIn("node-fixture expected '2.0.0', found '2.0.1'", stderr)

    def test_allows_missing_optional_or_platform_inapplicable_node_packages(
        self,
    ) -> None:
        lock = self._package_lock()
        packages = lock["packages"]
        assert isinstance(packages, dict)
        packages.update(
            {
                "node_modules/optional-missing": {
                    "version": "1.0.0",
                    "license": "MIT",
                    "optional": True,
                },
                "node_modules/linux-only": {
                    "version": "1.0.0",
                    "license": "MIT",
                    "os": ["linux"],
                },
                "node_modules/x64-only": {
                    "version": "1.0.0",
                    "license": "MIT",
                    "cpu": ["x64"],
                },
                "node_modules/not-darwin": {
                    "version": "1.0.0",
                    "license": "MIT",
                    "os": ["!darwin"],
                },
                "node_modules/not-arm64": {
                    "version": "1.0.0",
                    "license": "MIT",
                    "cpu": ["!arm64"],
                },
            }
        )
        self._replace_package_lock(lock)

        result, stderr = self._run_generate("build/notices")

        self.assertEqual(result, 0, stderr)
        inventory = json.loads(
            (self.repository / "build/notices/dependency-inventory.json").read_text()
        )
        self.assertEqual(
            [record["name"] for record in inventory["dependencies"]["node"]],
            ["node-fixture"],
        )

    def test_requires_node_package_allowed_by_negative_platform_constraints(
        self,
    ) -> None:
        lock = self._package_lock()
        packages = lock["packages"]
        assert isinstance(packages, dict)
        packages["node_modules/portable-missing"] = {
            "version": "1.0.0",
            "license": "MIT",
            "os": ["!win32"],
            "cpu": ["!x64"],
        }
        self._replace_package_lock(lock)

        result, stderr = self._run_generate("build/notices")

        self.assertEqual(result, 1)
        self.assertIn("portable-missing==1.0.0", stderr)

    def test_rejects_source_urls_that_can_carry_credentials(self) -> None:
        for source in (
            "http://example.com/ffmpeg",
            "https://user@example.com/ffmpeg",
            "https://example.com/ffmpeg?token=value",
            "https://example.com/ffmpeg#fragment",
        ):
            with (
                self.subTest(source=source),
                self.assertRaises(notices.ReleaseNoticeError),
            ):
                notices.validated_source_url(source)

    def test_fails_closed_without_python_runtime_license_text(self) -> None:
        existing = self.repository / "build/notices"
        existing.mkdir(parents=True)
        sentinel = existing / "keep.txt"
        sentinel.write_text("keep", encoding="utf-8")
        records = [
            {
                "name": "unlicensed-fixture",
                "version": "1.0.0",
                "license_expression": "MIT",
                "license": None,
                "license_files": [],
            }
        ]

        result, stderr = self._run_generate("build/notices", records)

        self.assertEqual(result, 1)
        self.assertIn(
            "Unresolved Python runtime dependency license text: "
            "unlicensed-fixture==1.0.0",
            stderr,
        )
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")

    def test_uses_exact_vetted_python_license_override(self) -> None:
        _, license_path, attribution_path = self._write_python_override(
            "unlicensed-fixture", "1.0.0"
        )
        records = [
            {
                "name": "unlicensed-fixture",
                "version": "1.0.0",
                "license_expression": None,
                "license": "UNKNOWN",
                "license_files": [],
            }
        ]

        result, stderr = self._run_generate("build/notices", records)

        self.assertEqual(result, 0, stderr)
        inventory = json.loads(
            (self.repository / "build/notices/dependency-inventory.json").read_text()
        )
        dependency = inventory["dependencies"]["python"][0]
        self.assertEqual(dependency["license"], "MIT")
        self.assertEqual(dependency["license_resolution"], "vetted_override")
        self.assertEqual(dependency["license_source"]["source_revision"], "c" * 40)
        bundled_license = (
            self.repository / "build/notices" / dependency["license_files"][0]
        )
        bundled_attribution = (
            self.repository / "build/notices" / dependency["attribution_files"][0]
        )
        self.assertEqual(bundled_license.read_bytes(), license_path.read_bytes())
        self.assertEqual(
            bundled_attribution.read_bytes(), attribution_path.read_bytes()
        )

    def test_rejects_tampered_python_license_override(self) -> None:
        manifest_path, _, _ = self._write_python_override("unlicensed-fixture", "1.0.0")
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["packages"][0]["license_sha256"] = "0" * 64
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

        result, stderr = self._run_generate(
            "build/notices",
            [
                {
                    "name": "unlicensed-fixture",
                    "version": "1.0.0",
                    "license_expression": None,
                    "license": "UNKNOWN",
                    "license_files": [],
                }
            ],
        )

        self.assertEqual(result, 1)
        self.assertIn("license file for unlicensed-fixture==1.0.0 SHA-256", stderr)

    def test_does_not_apply_python_license_override_to_another_version(self) -> None:
        self._write_python_override("unlicensed-fixture", "1.0.0")

        result, stderr = self._run_generate(
            "build/notices",
            [
                {
                    "name": "unlicensed-fixture",
                    "version": "1.0.1",
                    "license_expression": "MIT",
                    "license": None,
                    "license_files": [],
                }
            ],
        )

        self.assertEqual(result, 1)
        self.assertIn("unlicensed-fixture==1.0.1", stderr)

    def test_resolves_first_party_worker_against_project_license(self) -> None:
        records = [
            {
                "name": "local-transcript-worker",
                "version": "0.1.0",
                "license_expression": "Apache-2.0",
                "license": None,
                "license_files": [],
            }
        ]

        result, stderr = self._run_generate("build/notices", records)

        self.assertEqual(result, 0, stderr)
        inventory = json.loads(
            (self.repository / "build/notices/dependency-inventory.json").read_text()
        )
        dependency = inventory["dependencies"]["python"][0]
        self.assertEqual(dependency["license_resolution"], "project")
        self.assertEqual(dependency["license"], "Apache-2.0")
        self.assertTrue(dependency["license_files"])


if __name__ == "__main__":
    unittest.main()
