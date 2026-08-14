#!/usr/bin/env python3
"""Verify an immutable prepared SayTrace macOS release directory."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

from macos_release_evidence import (
    EVIDENCE_KEYS,
    HARDWARE_KEYS,
    REQUIRED_CHECKS,
)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def file_record(path: Path) -> dict[str, Any]:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"Prepared release asset must be an ordinary file: {path}")
    resolved = path.resolve(strict=True)
    return {
        "name": resolved.name,
        "size": resolved.stat().st_size,
        "sha256": sha256(resolved),
    }


def accepted_notary_result(path: Path, *, expected_name: str) -> str:
    if path.is_symlink() or not path.is_file():
        raise ValueError("Prepared notary result must be an ordinary JSON file.")
    result = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(result, dict) or result.get("status") != "Accepted":
        raise ValueError("Prepared notary result was not accepted by Apple.")
    if result.get("name") != expected_name:
        raise ValueError("Prepared notary result has the wrong artifact name.")
    submission_id = result.get("id")
    if not isinstance(submission_id, str) or not re.fullmatch(
        r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}",
        submission_id,
    ):
        raise ValueError("Prepared notary result has no valid submission ID.")
    return submission_id.lower()


def validate_acceptance_template(
    path: Path,
    *,
    version: str,
    source_revision: str,
    installer: dict[str, Any],
) -> None:
    if path.is_symlink() or not path.is_file():
        raise ValueError("Prepared acceptance template must be an ordinary file.")
    template = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(template, dict) or set(template) != EVIDENCE_KEYS:
        raise ValueError("Prepared acceptance template inventory is invalid.")
    expected_identity = {
        "schema_version": 2,
        "product": "SayTrace macOS physical acceptance",
        "version": version,
        "source_revision": source_revision,
        "installer": installer,
        "performed_at_utc": "REPLACE_WITH_ISO_8601_UTC_TIMESTAMP",
        "operator_confirmation": False,
    }
    if any(template.get(key) != value for key, value in expected_identity.items()):
        raise ValueError("Prepared acceptance template identity is invalid.")
    hardware = template.get("hardware")
    if not isinstance(hardware, dict) or set(hardware) != HARDWARE_KEYS:
        raise ValueError("Prepared acceptance template hardware is invalid.")
    if hardware != {
        "architecture": "arm64",
        "model_identifier": "REPLACE_WITH_MAC_MODEL_IDENTIFIER",
        "macos_version": "REPLACE_WITH_MACOS_VERSION",
    }:
        raise ValueError("Prepared acceptance template hardware is invalid.")
    checks = template.get("checks")
    if not isinstance(checks, dict) or checks != {
        name: "not_run" for name in REQUIRED_CHECKS
    }:
        raise ValueError("Prepared acceptance template checks are invalid.")


def validate_prepared_root(
    root: Path,
    *,
    version: str,
    source_revision: str,
    ffmpeg_version: str,
    runtime_manifest: Path | None = None,
) -> dict[str, Any]:
    if root.is_symlink() or not root.is_dir():
        raise ValueError("Prepared release root must be an ordinary directory.")
    canonical_root = root.resolve(strict=True)
    state_root = canonical_root / ".release-state"
    expected_top_level = {
        ".release-state",
        "RELEASE_NOTES.md",
        f"SayTrace-{version}-ffmpeg-{ffmpeg_version}-source.tar.xz",
        f"SayTrace-{version}-ffmpeg-{ffmpeg_version}-source.tar.xz.asc",
        f"SayTrace-{version}-macos-arm64.dmg",
        "physical-acceptance-template.json",
        "prepared-release.json",
    }
    actual_top_level = {entry.name for entry in canonical_root.iterdir()}
    if actual_top_level != expected_top_level:
        raise ValueError("Prepared release top-level inventory is not exact.")
    if state_root.is_symlink() or not state_root.is_dir():
        raise ValueError("Prepared release state must be an ordinary directory.")
    expected_state = {
        f"SayTrace-{version}-macos-arm64.zip",
        "app-notarization.json",
        "dmg-notarization.json",
    }
    if {entry.name for entry in state_root.iterdir()} != expected_state:
        raise ValueError("Prepared release internal-state inventory is not exact.")

    manifest_path = canonical_root / "prepared-release.json"
    if manifest_path.is_symlink() or not manifest_path.is_file():
        raise ValueError("Prepared release manifest must be an ordinary file.")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if not isinstance(manifest, dict):
        raise TypeError("Prepared release manifest must be a JSON object.")
    expected_identity = {
        "schema_version": 1,
        "product": "SayTrace macOS prepared release",
        "version": version,
        "source_revision": source_revision,
        "state": "awaiting_exact_installer_physical_acceptance",
    }
    if any(manifest.get(key) != value for key, value in expected_identity.items()):
        raise ValueError("Prepared release manifest identity does not match.")
    distribution = manifest.get("distribution")
    if not isinstance(distribution, dict) or any(
        distribution.get(key) != value
        for key, value in {
            "official": False,
            "developer_id_signed": True,
            "notarized": True,
            "stapled": True,
        }.items()
    ):
        raise ValueError("Prepared release distribution state is invalid.")
    for key in ("app_notarization_id", "dmg_notarization_id"):
        if not isinstance(distribution.get(key), str) or not re.fullmatch(
            r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}",
            distribution[key],
        ):
            raise ValueError(f"Prepared release {key} is invalid.")

    expected_assets = {
        "installer": canonical_root / f"SayTrace-{version}-macos-arm64.dmg",
        "app_notary_payload": state_root / f"SayTrace-{version}-macos-arm64.zip",
        "app_notary_result": state_root / "app-notarization.json",
        "dmg_notary_result": state_root / "dmg-notarization.json",
        "ffmpeg_source": canonical_root
        / f"SayTrace-{version}-ffmpeg-{ffmpeg_version}-source.tar.xz",
        "ffmpeg_source_signature": canonical_root
        / f"SayTrace-{version}-ffmpeg-{ffmpeg_version}-source.tar.xz.asc",
        "release_notes": canonical_root / "RELEASE_NOTES.md",
    }
    assets = manifest.get("assets")
    if not isinstance(assets, dict) or set(assets) != set(expected_assets):
        raise ValueError("Prepared release asset inventory is invalid.")
    for name, path in expected_assets.items():
        if assets.get(name) != file_record(path):
            raise ValueError(f"Prepared release asset digest mismatch: {name}")

    app_id = accepted_notary_result(
        state_root / "app-notarization.json",
        expected_name=f"SayTrace-{version}-macos-arm64.zip",
    )
    dmg_id = accepted_notary_result(
        state_root / "dmg-notarization.json",
        expected_name=f"SayTrace-{version}-macos-arm64.dmg",
    )
    if app_id == dmg_id:
        raise ValueError("Prepared app and DMG notarization IDs must be distinct.")
    if distribution.get("app_notarization_id") != app_id:
        raise ValueError("Prepared app notarization ID does not match its result.")
    if distribution.get("dmg_notarization_id") != dmg_id:
        raise ValueError("Prepared DMG notarization ID does not match its result.")

    validate_acceptance_template(
        canonical_root / "physical-acceptance-template.json",
        version=version,
        source_revision=source_revision,
        installer=assets["installer"],
    )

    if runtime_manifest is not None:
        if runtime_manifest.is_symlink() or not runtime_manifest.is_file():
            raise ValueError("Mounted runtime manifest must be an ordinary file.")
        runtime_record = manifest.get("runtime_manifest")
        if not isinstance(runtime_record, dict):
            raise TypeError("Prepared runtime-manifest record is missing.")
        if runtime_record != {
            "name": "runtime-manifest.json",
            "sha256": sha256(runtime_manifest.resolve(strict=True)),
        }:
            raise ValueError(
                "Mounted runtime manifest does not match the prepared app."
            )
    return manifest


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prepared-root", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--ffmpeg-version", required=True)
    parser.add_argument("--runtime-manifest", type=Path)
    args = parser.parse_args()
    validate_prepared_root(
        args.prepared_root,
        version=args.version,
        source_revision=args.source_revision,
        ffmpeg_version=args.ffmpeg_version,
        runtime_manifest=args.runtime_manifest,
    )
    print("Verified immutable prepared macOS release inputs.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
