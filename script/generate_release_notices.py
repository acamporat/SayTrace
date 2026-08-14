#!/usr/bin/env python3
"""Generate a deterministic legal-notice bundle for a SayTrace release."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Iterable
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit, urlunsplit

import tomllib


class ReleaseNoticeError(RuntimeError):
    """A release-notice input or dependency inventory is invalid."""


PROJECT_NOTICE_FILES = ("LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.md")
MODEL_MANIFEST_FILES = (
    "worker/model-manifest.json",
    "worker/model-manifest.macos.json",
)
PYTHON_LICENSE_OVERRIDES_FILE = "third_party/python-license-overrides.json"
PYTHON_LICENSE_ROOT = "third_party/licenses/python"
PYTHON_OVERRIDE_FIELDS = {
    "attribution_file",
    "attribution_sha256",
    "distribution_filename",
    "distribution_sha256",
    "license",
    "license_file",
    "license_sha256",
    "license_url",
    "name",
    "package_index_url",
    "source_revision",
    "source_url",
    "version",
}
LEGAL_FILE_PATTERN = re.compile(
    r"^(?:licen[cs]e|copying|notice)(?:$|[._-].*)", re.IGNORECASE
)
SAFE_FILENAME_PATTERN = re.compile(r"[^A-Za-z0-9._+-]+")
PACKAGE_NAME_PATTERN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")
SECRET_KEY_PATTERN = re.compile(
    r"(?:token|password|secret|credential|authorization)", re.IGNORECASE
)
NODE_RELEASE_OS = "darwin"
NODE_RELEASE_CPU = "arm64"


PYTHON_INVENTORY_PROGRAM = r"""
import importlib.metadata as metadata
import json
import pathlib
import re

legal = re.compile(r"^(?:licen[cs]e|copying|notice)(?:$|[._-].*)", re.I)
records = []
for distribution in metadata.distributions():
    name = distribution.metadata.get("Name")
    version = distribution.version
    if not name or not version:
        continue
    license_files = []
    for item in distribution.files or ():
        item_text = str(item)
        if not legal.match(pathlib.PurePosixPath(item_text.replace("\\", "/")).name):
            continue
        candidate = pathlib.Path(distribution.locate_file(item))
        if candidate.is_file():
            license_files.append({"relative": item_text, "path": str(candidate.resolve())})
    records.append({
        "name": name,
        "version": version,
        "license_expression": distribution.metadata.get("License-Expression"),
        "license": distribution.metadata.get("License"),
        "license_files": sorted(
            license_files,
            key=lambda item: (item["relative"].lower(), item["relative"]),
        ),
    })
print(json.dumps(records, sort_keys=True, separators=(",", ":")))
"""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text.rstrip() + "\n", encoding="utf-8", newline="\n")
    path.chmod(0o644)


def write_json(path: Path, payload: Any) -> None:
    write_text(path, json.dumps(payload, indent=2, sort_keys=True, ensure_ascii=False))


def copy_file(source: Path, destination: Path) -> None:
    if not source.is_file():
        raise ReleaseNoticeError(f"Required notice input is missing: {source.name}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o644)


def safe_filename(value: str, *, maximum: int = 120) -> str:
    normalized = SAFE_FILENAME_PATTERN.sub("_", value.strip()).strip("._")
    if not normalized or normalized in {".", ".."}:
        normalized = "component"
    if len(normalized) > maximum:
        suffix = hashlib.sha256(value.encode("utf-8")).hexdigest()[:12]
        normalized = f"{normalized[: maximum - len(suffix) - 1]}-{suffix}"
    return normalized


def concise_license(value: Any) -> str:
    if not isinstance(value, str):
        return "UNKNOWN"
    normalized = " ".join(value.split())
    if not normalized or len(normalized) > 240:
        return "UNKNOWN"
    return normalized


def validated_source_url(value: str) -> str:
    parsed = urlsplit(value)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username
        or parsed.password
        or parsed.query
        or parsed.fragment
        or SECRET_KEY_PATTERN.search(parsed.path)
    ):
        raise ReleaseNoticeError(
            "FFmpeg source provenance must be an HTTPS URL without credentials, query, or fragment"
        )
    host = parsed.hostname.lower()
    if parsed.port:
        host = f"{host}:{parsed.port}"
    return urlunsplit(("https", host, parsed.path or "/", "", ""))


def normalized_package_name(value: str) -> str:
    return re.sub(r"[-_.]+", "-", value).lower()


def validated_override_url(value: Any, description: str) -> str:
    if not isinstance(value, str):
        raise ReleaseNoticeError(f"Python license override {description} is invalid")
    parsed = urlsplit(value)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username
        or parsed.password
        or parsed.query
        or parsed.fragment
        or SECRET_KEY_PATTERN.search(parsed.path)
    ):
        raise ReleaseNoticeError(
            f"Python license override {description} must be an HTTPS URL without "
            "credentials, query, or fragment"
        )
    host = parsed.hostname.lower()
    if parsed.port:
        host = f"{host}:{parsed.port}"
    return urlunsplit(("https", host, parsed.path or "/", "", ""))


def validated_override_file(
    repository: Path,
    relative_value: Any,
    expected_sha256: Any,
    description: str,
) -> tuple[str, Path]:
    if not isinstance(relative_value, str) or not relative_value:
        raise ReleaseNoticeError(f"Python license override {description} is invalid")
    relative = Path(relative_value)
    if (
        relative.is_absolute()
        or relative.as_posix() != relative_value
        or any(part in {"", ".", ".."} for part in relative.parts)
    ):
        raise ReleaseNoticeError(
            f"Python license override {description} must be a canonical relative path"
        )
    if not isinstance(expected_sha256, str) or not SHA256_PATTERN.fullmatch(
        expected_sha256
    ):
        raise ReleaseNoticeError(
            f"Python license override {description} SHA-256 is invalid"
        )

    try:
        allowed_root = (repository / PYTHON_LICENSE_ROOT).resolve(strict=True)
    except FileNotFoundError as error:
        raise ReleaseNoticeError(
            f"Python license override root does not exist: {PYTHON_LICENSE_ROOT}"
        ) from error
    candidate = repository
    for part in relative.parts:
        candidate /= part
        if candidate.is_symlink():
            raise ReleaseNoticeError(
                f"Python license override {description} cannot use symbolic links"
            )
    try:
        resolved = candidate.resolve(strict=True)
    except FileNotFoundError as error:
        raise ReleaseNoticeError(
            f"Python license override {description} does not exist"
        ) from error
    if not resolved.is_file() or not resolved.is_relative_to(allowed_root):
        raise ReleaseNoticeError(
            f"Python license override {description} must be a file beneath "
            f"{PYTHON_LICENSE_ROOT}"
        )
    if sha256_file(resolved) != expected_sha256:
        raise ReleaseNoticeError(
            f"Python license override {description} SHA-256 does not match"
        )
    return relative_value, resolved


def load_python_license_overrides(
    repository: Path,
) -> dict[tuple[str, str], dict[str, Any]]:
    manifest_path = repository / PYTHON_LICENSE_OVERRIDES_FILE
    if manifest_path.is_symlink():
        raise ReleaseNoticeError(
            "Python license override manifest must be an ordinary file"
        )
    if not manifest_path.exists():
        return {}
    if not manifest_path.is_file():
        raise ReleaseNoticeError(
            "Python license override manifest must be an ordinary file"
        )
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise ReleaseNoticeError(
            "Python license override manifest is not valid JSON"
        ) from error
    if not isinstance(manifest, dict) or set(manifest) != {
        "packages",
        "schema_version",
    }:
        raise ReleaseNoticeError("Python license override manifest has invalid fields")
    if manifest.get("schema_version") != 1 or not isinstance(
        manifest.get("packages"), list
    ):
        raise ReleaseNoticeError("Python license override manifest schema is invalid")

    overrides: dict[tuple[str, str], dict[str, Any]] = {}
    for item in manifest["packages"]:
        if not isinstance(item, dict) or set(item) != PYTHON_OVERRIDE_FIELDS:
            raise ReleaseNoticeError("Python license override entry has invalid fields")
        name = item.get("name")
        version = item.get("version")
        if not isinstance(name, str) or not PACKAGE_NAME_PATTERN.fullmatch(name):
            raise ReleaseNoticeError("Python license override package name is invalid")
        if (
            not isinstance(version, str)
            or not version
            or version != version.strip()
            or any(character.isspace() for character in version)
        ):
            raise ReleaseNoticeError(
                f"Python license override version is invalid for {name}"
            )
        key = (normalized_package_name(name), version)
        if key in overrides:
            raise ReleaseNoticeError(
                f"Duplicate Python license override for {name}=={version}"
            )

        license_value = concise_license(item.get("license"))
        if license_value == "UNKNOWN":
            raise ReleaseNoticeError(
                f"Python license override must identify a license for {name}=={version}"
            )
        license_relative, license_source = validated_override_file(
            repository,
            item.get("license_file"),
            item.get("license_sha256"),
            f"license file for {name}=={version}",
        )
        if not LEGAL_FILE_PATTERN.match(license_source.name):
            raise ReleaseNoticeError(
                f"Python license override file is not a legal notice for {name}=={version}"
            )
        attribution_relative, attribution_source = validated_override_file(
            repository,
            item.get("attribution_file"),
            item.get("attribution_sha256"),
            f"attribution file for {name}=={version}",
        )
        if attribution_source.name.casefold() == license_source.name.casefold():
            raise ReleaseNoticeError(
                f"Python license override files must have distinct names for "
                f"{name}=={version}"
            )
        source_revision = item.get("source_revision")
        if not isinstance(source_revision, str) or not re.fullmatch(
            r"[0-9a-f]{40}", source_revision
        ):
            raise ReleaseNoticeError(
                f"Python license override source revision is invalid for {name}=={version}"
            )
        source_url = validated_override_url(
            item.get("source_url"), f"source URL for {name}=={version}"
        )
        license_url = validated_override_url(
            item.get("license_url"), f"license URL for {name}=={version}"
        )
        if source_revision not in urlsplit(license_url).path:
            raise ReleaseNoticeError(
                f"Python license override URL is not pinned to its revision for "
                f"{name}=={version}"
            )
        package_index_url = validated_override_url(
            item.get("package_index_url"),
            f"package-index URL for {name}=={version}",
        )
        distribution_filename = item.get("distribution_filename")
        if (
            not isinstance(distribution_filename, str)
            or not distribution_filename
            or Path(distribution_filename).name != distribution_filename
            or safe_filename(distribution_filename) != distribution_filename
        ):
            raise ReleaseNoticeError(
                f"Python license override distribution filename is invalid for "
                f"{name}=={version}"
            )
        distribution_sha256 = item.get("distribution_sha256")
        if not isinstance(distribution_sha256, str) or not SHA256_PATTERN.fullmatch(
            distribution_sha256
        ):
            raise ReleaseNoticeError(
                f"Python license override distribution SHA-256 is invalid for "
                f"{name}=={version}"
            )

        overrides[key] = {
            "attribution_file": attribution_relative,
            "attribution_sha256": item["attribution_sha256"],
            "attribution_source": attribution_source,
            "distribution_filename": distribution_filename,
            "distribution_sha256": distribution_sha256,
            "license": license_value,
            "license_file": license_relative,
            "license_sha256": item["license_sha256"],
            "license_source": license_source,
            "license_url": license_url,
            "name": name,
            "package_index_url": package_index_url,
            "source_revision": source_revision,
            "source_url": source_url,
            "version": version,
        }
    return overrides


def python_project_metadata(repository: Path) -> dict[str, str]:
    path = repository / "worker/pyproject.toml"
    try:
        metadata = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ReleaseNoticeError("worker/pyproject.toml is invalid") from error
    project = metadata.get("project")
    if not isinstance(project, dict):
        raise ReleaseNoticeError("worker/pyproject.toml has no project metadata")
    name = project.get("name")
    version = project.get("version")
    license_value = project.get("license")
    if isinstance(license_value, dict):
        license_value = license_value.get("text")
    if not isinstance(name, str) or not PACKAGE_NAME_PATTERN.fullmatch(name):
        raise ReleaseNoticeError("worker project package name is invalid")
    if not isinstance(version, str) or not version:
        raise ReleaseNoticeError("worker project package version is invalid")
    license_text = concise_license(license_value)
    if license_text == "UNKNOWN":
        raise ReleaseNoticeError("worker project package license is unresolved")
    return {"license": license_text, "name": name, "version": version}


def read_single_line(path: Path, description: str) -> str:
    if not path.is_file():
        raise ReleaseNoticeError(f"FFmpeg release input is missing {description}")
    lines = path.read_text(encoding="utf-8").splitlines()
    if len(lines) != 1 or not lines[0].strip():
        raise ReleaseNoticeError(f"FFmpeg {description} must contain exactly one value")
    return lines[0].strip()


def ffmpeg_source_provenance(
    repository: Path, prefix: Path, source_argument: str
) -> tuple[Path | None, dict[str, str]]:
    provenance_root = prefix / "share/saytrace-ffmpeg"
    staged_provenance = {
        "source_url": validated_source_url(
            read_single_line(provenance_root / "source-url.txt", "source URL")
        ),
        "source_sha256": read_single_line(
            provenance_root / "source-sha256.txt", "source SHA-256"
        ).lower(),
        "source_signing_key_fingerprint": read_single_line(
            provenance_root / "source-signing-key-fingerprint.txt",
            "source-signing key fingerprint",
        ).upper(),
    }
    if not re.fullmatch(r"[0-9a-f]{64}", staged_provenance["source_sha256"]):
        raise ReleaseNoticeError("FFmpeg source SHA-256 is invalid")
    if not re.fullmatch(
        r"[0-9A-F]{40}", staged_provenance["source_signing_key_fingerprint"]
    ):
        raise ReleaseNoticeError("FFmpeg source-signing key fingerprint is invalid")

    if source_argument.startswith(("https://", "http://")):
        requested_url = validated_source_url(source_argument)
        if requested_url != staged_provenance["source_url"]:
            raise ReleaseNoticeError(
                "FFmpeg source URL does not match the verified build provenance"
            )
        return None, staged_provenance

    source = Path(source_argument)
    if not source.is_absolute():
        source = repository / source
    source = source.resolve(strict=True)
    if not source.is_dir():
        raise ReleaseNoticeError("FFmpeg source input is not a directory")
    return source, staged_provenance


def sanitize_cargo_source(source: Any) -> str:
    if not isinstance(source, str) or not source:
        return "workspace"
    prefix, separator, remainder = source.partition("+")
    if not separator or prefix not in {"registry", "git"}:
        return "external"
    parsed = urlsplit(remainder)
    if parsed.scheme not in {"https", "http"} or not parsed.hostname:
        return prefix
    host = parsed.hostname.lower()
    if parsed.port:
        host = f"{host}:{parsed.port}"
    fragment = (
        parsed.fragment if re.fullmatch(r"[0-9a-fA-F]{7,64}", parsed.fragment) else ""
    )
    sanitized = urlunsplit((parsed.scheme, host, parsed.path, "", fragment))
    return f"{prefix}+{sanitized}"


def sanitize_configuration(configuration: str) -> str:
    try:
        tokens = shlex.split(configuration)
    except ValueError as error:
        raise ReleaseNoticeError("FFmpeg configuration could not be parsed") from error

    sanitized: list[str] = []
    for token in tokens:
        if "=" not in token:
            sanitized.append(token)
            continue
        key, value = token.split("=", 1)
        if SECRET_KEY_PATTERN.search(key):
            sanitized.append(f"{key}=<redacted>")
        elif value.startswith(("/", "-I/", "-L/")):
            sanitized.append(f"{key}=<local-path>")
        else:
            sanitized.append(token)
    return " ".join(sanitized)


def assert_safe_generated_metadata(value: Any, forbidden_paths: Iterable[Path]) -> None:
    forbidden = [str(path.resolve(strict=False)) for path in forbidden_paths]

    def visit(item: Any) -> None:
        if isinstance(item, dict):
            for nested in item.values():
                visit(nested)
            return
        if isinstance(item, (list, tuple)):
            for nested in item:
                visit(nested)
            return
        if not isinstance(item, str):
            return
        if any(path and path in item for path in forbidden):
            raise ReleaseNoticeError(
                "Generated metadata contains a local filesystem path"
            )
        if re.search(r"(?<!:)/(?:Users|private|tmp|opt|home)/", item):
            raise ReleaseNoticeError(
                "Generated metadata contains an absolute local path"
            )
        if re.search(r"https?://[^/\s]+@", item, re.IGNORECASE):
            raise ReleaseNoticeError("Generated metadata contains URL credentials")
        if re.search(
            r"(?:token|password|secret|credential|authorization)\s*[:=](?!<redacted>)",
            item,
            re.IGNORECASE,
        ):
            raise ReleaseNoticeError(
                "Generated metadata contains a credential-like value"
            )

    visit(value)


def validate_repository(repository_root: Path) -> Path:
    repository = repository_root.resolve(strict=True)
    if not repository.is_dir():
        raise ReleaseNoticeError("Repository root is not a directory")
    required = (
        *PROJECT_NOTICE_FILES,
        "package-lock.json",
        "src-tauri/Cargo.toml",
        "worker/pyproject.toml",
    )
    for relative in required:
        if not (repository / relative).is_file():
            raise ReleaseNoticeError(
                f"Repository is missing required release input: {relative}"
            )
    return repository


def validate_output_path(repository: Path, output_argument: Path) -> Path:
    build = repository / "build"
    if build.is_symlink() or (build.exists() and not build.is_dir()):
        raise ReleaseNoticeError("Repository build path must be an ordinary directory")
    build.mkdir(mode=0o755, exist_ok=True)

    candidate = (
        output_argument
        if output_argument.is_absolute()
        else repository / output_argument
    )
    candidate = Path(os.path.abspath(candidate))
    try:
        relative = candidate.relative_to(build)
    except ValueError as error:
        raise ReleaseNoticeError(
            "Output must be located beneath the repository build directory"
        ) from error
    if not relative.parts:
        raise ReleaseNoticeError(
            "Output cannot replace the repository build directory itself"
        )

    current = build
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            raise ReleaseNoticeError("Output path cannot contain symbolic links")

    candidate.parent.mkdir(parents=True, exist_ok=True)
    resolved_build = build.resolve(strict=True)
    resolved_output = candidate.resolve(strict=False)
    if resolved_output == resolved_build or not resolved_output.is_relative_to(
        resolved_build
    ):
        raise ReleaseNoticeError(
            "Output resolves outside the repository build directory"
        )
    if candidate.exists() and not candidate.is_dir():
        raise ReleaseNoticeError("Existing release-notice output is not a directory")
    return candidate


def legal_files_at(package_root: Path) -> list[Path]:
    try:
        canonical_root = package_root.resolve(strict=True)
    except FileNotFoundError:
        return []
    if not canonical_root.is_dir():
        return []

    result: list[Path] = []
    for candidate in sorted(
        canonical_root.iterdir(), key=lambda path: (path.name.lower(), path.name)
    ):
        if not LEGAL_FILE_PATTERN.match(candidate.name):
            continue
        try:
            resolved = candidate.resolve(strict=True)
        except (FileNotFoundError, RuntimeError):
            continue
        if resolved.is_file() and resolved.is_relative_to(canonical_root):
            result.append(resolved)
    return result


def unique_paths(paths: Iterable[Path]) -> list[Path]:
    by_path: dict[str, Path] = {}
    for path in paths:
        try:
            resolved = path.resolve(strict=True)
        except (FileNotFoundError, RuntimeError):
            continue
        if resolved.is_file():
            by_path[str(resolved)] = resolved
    return [by_path[key] for key in sorted(by_path)]


def component_directory(
    ecosystem: str,
    name: str,
    version: str,
    output: Path,
    identity: str | None = None,
) -> Path:
    identity = identity or f"{name}\0{version}"
    suffix = hashlib.sha256(identity.encode("utf-8")).hexdigest()[:8]
    return (
        output
        / "licenses"
        / ecosystem
        / f"{safe_filename(f'{name}-{version}')}-{suffix}"
    )


def copy_component_licenses(
    ecosystem: str,
    name: str,
    version: str,
    candidates: Iterable[Path],
    output: Path,
    identity: str | None = None,
) -> list[str]:
    destination_root = component_directory(ecosystem, name, version, output, identity)
    copied: list[str] = []
    used_names: dict[str, str] = {}
    for source in unique_paths(candidates):
        basename = safe_filename(source.name, maximum=100)
        digest = sha256_file(source)
        prior_digest = used_names.get(basename.lower())
        if prior_digest == digest:
            continue
        if prior_digest is not None:
            stem, suffix = os.path.splitext(basename)
            basename = f"{stem}-{digest[:12]}{suffix}"
        used_names[basename.lower()] = digest
        destination = destination_root / basename
        copy_file(source, destination)
        copied.append(destination.relative_to(output).as_posix())
    return sorted(copied)


def node_constraint_allows(
    target: str,
    constraint: Any,
    *,
    field: str,
    lock_path: str,
) -> bool:
    """Apply npm's positive/negative os and cpu list semantics."""

    if constraint is None:
        return True
    values = [constraint] if isinstance(constraint, str) else constraint
    if (
        not isinstance(values, list)
        or not values
        or any(
            not isinstance(value, str)
            or not value
            or value == "!"
            or value != value.strip()
            for value in values
        )
    ):
        raise ReleaseNoticeError(
            f"Invalid Node {field} constraint for locked package {lock_path}"
        )

    included = {value for value in values if not value.startswith("!")}
    excluded = {value[1:] for value in values if value.startswith("!")}
    if target in excluded:
        return False
    return not included or "any" in included or target in included


def node_package_applies_to_release(locked: dict[str, Any], lock_path: str) -> bool:
    return node_constraint_allows(
        NODE_RELEASE_OS,
        locked.get("os"),
        field="os",
        lock_path=lock_path,
    ) and node_constraint_allows(
        NODE_RELEASE_CPU,
        locked.get("cpu"),
        field="cpu",
        lock_path=lock_path,
    )


def node_inventory(repository: Path, output: Path) -> list[dict[str, Any]]:
    lock = json.loads((repository / "package-lock.json").read_text(encoding="utf-8"))
    packages = lock.get("packages")
    if not isinstance(packages, dict):
        raise ReleaseNoticeError(
            "package-lock.json does not contain a packages inventory"
        )

    resolved: dict[tuple[str, str], dict[str, Any]] = {}
    roots: dict[tuple[str, str], list[Path]] = {}
    for lock_path, locked in sorted(packages.items()):
        if not isinstance(lock_path, str) or "node_modules/" not in lock_path:
            continue
        if not isinstance(locked, dict):
            raise ReleaseNoticeError(
                f"Invalid Node lockfile package record: {lock_path}"
            )
        fallback_name = lock_path.rsplit("node_modules/", 1)[-1]
        locked_name = locked.get("name")
        if locked_name is None:
            expected_name = fallback_name
        elif isinstance(locked_name, str) and locked_name:
            expected_name = locked_name
        else:
            raise ReleaseNoticeError(f"Invalid Node lockfile package name: {lock_path}")
        expected_version = locked.get("version")
        if not isinstance(expected_version, str) or not expected_version:
            raise ReleaseNoticeError(
                f"Invalid Node lockfile package version: {lock_path}"
            )

        is_optional = locked.get("optional") is True
        required = not is_optional and node_package_applies_to_release(
            locked, lock_path
        )
        package_root = repository / lock_path
        metadata_path = package_root / "package.json"
        if not metadata_path.is_file():
            if required:
                raise ReleaseNoticeError(
                    "Required locked Node package is not installed: "
                    f"{expected_name}=={expected_version} ({lock_path})"
                )
            continue
        try:
            loaded = json.loads(metadata_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise ReleaseNoticeError(
                f"Installed Node package metadata is invalid: {lock_path}/package.json"
            ) from error
        if not isinstance(loaded, dict):
            raise ReleaseNoticeError(
                f"Installed Node package metadata is invalid: {lock_path}/package.json"
            )
        actual_name = loaded.get("name")
        actual_version = loaded.get("version")
        if actual_name != expected_name:
            raise ReleaseNoticeError(
                "Installed Node package name does not match package-lock.json: "
                f"expected {expected_name!r}, found {actual_name!r} ({lock_path})"
            )
        if actual_version != expected_version:
            raise ReleaseNoticeError(
                "Installed Node package version does not match package-lock.json: "
                f"{expected_name} expected {expected_version!r}, "
                f"found {actual_version!r} ({lock_path})"
            )
        key = (expected_name, expected_version)
        license_value = concise_license(loaded.get("license") or locked.get("license"))
        record = resolved.setdefault(
            key,
            {
                "name": expected_name,
                "version": expected_version,
                "license": license_value,
            },
        )
        if record["license"] == "UNKNOWN" and license_value != "UNKNOWN":
            record["license"] = license_value
        roots.setdefault(key, []).append(package_root)

    records: list[dict[str, Any]] = []
    for key in sorted(resolved, key=lambda item: (item[0].lower(), item[0], item[1])):
        record = dict(resolved[key])
        candidates = [path for root in roots[key] for path in legal_files_at(root)]
        record["license_files"] = copy_component_licenses(
            "node", record["name"], record["version"], candidates, output
        )
        records.append(record)
    return records


def cargo_executable() -> str:
    configured = os.environ.get("CARGO")
    candidates = [
        configured,
        shutil.which("cargo"),
        "/opt/homebrew/opt/rustup/bin/cargo",
        str(Path.home() / ".cargo/bin/cargo"),
    ]
    for candidate in candidates:
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return candidate
    raise ReleaseNoticeError(
        "cargo is required to generate the Rust dependency inventory"
    )


def rust_inventory(repository: Path, output: Path) -> list[dict[str, Any]]:
    cargo = cargo_executable()
    command = [
        cargo,
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--filter-platform",
        "aarch64-apple-darwin",
        "--manifest-path",
        str(repository / "src-tauri/Cargo.toml"),
    ]
    result = subprocess.run(
        command,
        cwd=repository,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env={
            **os.environ,
            "PATH": os.pathsep.join(
                (str(Path(cargo).parent), os.environ.get("PATH", ""))
            ),
        },
    )
    if result.returncode != 0:
        raise ReleaseNoticeError(
            "cargo metadata failed for the locked Rust dependency graph"
        )
    try:
        metadata = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ReleaseNoticeError("cargo metadata returned invalid JSON") from error

    workspace_members = set(metadata.get("workspace_members") or ())
    packages = metadata.get("packages")
    if not isinstance(packages, list):
        raise ReleaseNoticeError("cargo metadata did not contain a package inventory")

    included_ids: set[str] | None = None
    resolve = metadata.get("resolve")
    if isinstance(resolve, dict) and isinstance(resolve.get("nodes"), list):
        adjacency: dict[str, list[str]] = {}
        for node in resolve["nodes"]:
            if not isinstance(node, dict) or not isinstance(node.get("id"), str):
                continue
            adjacency[node["id"]] = [
                dependency["pkg"]
                for dependency in node.get("deps") or ()
                if isinstance(dependency, dict)
                and isinstance(dependency.get("pkg"), str)
            ]
        included_ids = set(workspace_members)
        pending = list(workspace_members)
        while pending:
            package_id = pending.pop()
            for dependency_id in adjacency.get(package_id, ()):
                if dependency_id not in included_ids:
                    included_ids.add(dependency_id)
                    pending.append(dependency_id)

    records: list[dict[str, Any]] = []
    for package in sorted(
        (item for item in packages if isinstance(item, dict)),
        key=lambda item: (
            str(item.get("name", "")).lower(),
            str(item.get("version", "")),
        ),
    ):
        if package.get("id") in workspace_members:
            continue
        if included_ids is not None and package.get("id") not in included_ids:
            continue
        name = str(package.get("name") or "UNKNOWN")
        version = str(package.get("version") or "UNKNOWN")
        source = sanitize_cargo_source(package.get("source"))
        manifest_path = package.get("manifest_path")
        candidates = (
            legal_files_at(Path(manifest_path).parent)
            if isinstance(manifest_path, str)
            else []
        )
        records.append(
            {
                "name": name,
                "version": version,
                "license": concise_license(package.get("license")),
                "source": source,
                "license_files": copy_component_licenses(
                    "rust",
                    name,
                    version,
                    candidates,
                    output,
                    identity=f"{name}\0{version}\0{source}",
                ),
            }
        )
    return records


def python_inventory(
    python: Path,
    output: Path,
    *,
    repository: Path | None = None,
) -> list[dict[str, Any]]:
    if not python.is_file() or not os.access(python, os.X_OK):
        raise ReleaseNoticeError(
            "Selected Python interpreter is missing or not executable"
        )
    result = subprocess.run(
        [str(python), "-c", PYTHON_INVENTORY_PROGRAM],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseNoticeError("Python dependency inventory failed")
    try:
        distributions = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ReleaseNoticeError(
            "Python dependency inventory returned invalid JSON"
        ) from error
    if not isinstance(distributions, list):
        raise ReleaseNoticeError(
            "Python dependency inventory returned an invalid record set"
        )

    merged: dict[tuple[str, str], dict[str, Any]] = {}
    for distribution in distributions:
        if not isinstance(distribution, dict):
            continue
        name = str(distribution.get("name") or "UNKNOWN")
        version = str(distribution.get("version") or "UNKNOWN")
        key = (normalized_package_name(name), version)
        record = merged.setdefault(
            key,
            {
                "name": name,
                "version": version,
                "license": concise_license(
                    distribution.get("license_expression")
                    or distribution.get("license")
                ),
                "candidates": [],
            },
        )
        for item in distribution.get("license_files") or ():
            if isinstance(item, dict) and isinstance(item.get("path"), str):
                candidate = Path(item["path"])
                if LEGAL_FILE_PATTERN.match(candidate.name):
                    record["candidates"].append(candidate)

    overrides = load_python_license_overrides(repository) if repository else {}
    project = python_project_metadata(repository) if repository else None
    project_key = (
        (normalized_package_name(project["name"]), project["version"])
        if project
        else None
    )

    records: list[dict[str, Any]] = []
    unresolved: list[str] = []
    for key in sorted(merged):
        source = merged[key]
        name = source["name"]
        version = source["version"]
        record: dict[str, Any] = {
            "attribution_files": [],
            "license": source["license"],
            "license_files": copy_component_licenses(
                "python",
                name,
                version,
                source["candidates"],
                output,
            ),
            "name": name,
            "version": version,
        }
        if record["license_files"]:
            record["license_resolution"] = "distribution"
        elif project_key == key and project is not None and repository is not None:
            record["license"] = project["license"]
            record["license_files"] = copy_component_licenses(
                "python",
                name,
                version,
                (repository / "LICENSE", repository / "NOTICE"),
                output,
            )
            record["license_resolution"] = "project"
        elif key in overrides:
            override = overrides[key]
            declared_license = record["license"]
            if (
                declared_license != "UNKNOWN"
                and declared_license.casefold() != override["license"].casefold()
            ):
                raise ReleaseNoticeError(
                    f"Python license override conflicts with package metadata for "
                    f"{name}=={version}"
                )
            record["license"] = override["license"]
            record["license_files"] = copy_component_licenses(
                "python",
                name,
                version,
                (override["license_source"],),
                output,
            )
            destination_root = component_directory("python", name, version, output)
            attribution_destination = destination_root / safe_filename(
                override["attribution_source"].name
            )
            copy_file(override["attribution_source"], attribution_destination)
            record["attribution_files"] = [
                attribution_destination.relative_to(output).as_posix()
            ]
            record["license_resolution"] = "vetted_override"
            record["license_source"] = {
                "distribution_filename": override["distribution_filename"],
                "distribution_sha256": override["distribution_sha256"],
                "license_sha256": override["license_sha256"],
                "license_url": override["license_url"],
                "manifest": PYTHON_LICENSE_OVERRIDES_FILE,
                "package_index_url": override["package_index_url"],
                "source_revision": override["source_revision"],
                "source_url": override["source_url"],
            }
        else:
            record["license_resolution"] = "unresolved"
            unresolved.append(f"{name}=={version}")
        records.append(record)

    if unresolved:
        packages = ", ".join(sorted(unresolved, key=str.casefold))
        raise ReleaseNoticeError(
            "Unresolved Python runtime dependency license text: "
            f"{packages}. Add the package's distributed legal file or an exact, "
            f"vetted entry in {PYTHON_LICENSE_OVERRIDES_FILE}."
        )
    return records


def find_ffmpeg_input(prefix: Path, filename: str) -> Path:
    matches = sorted(
        (path for path in prefix.rglob(filename) if path.is_file()),
        key=lambda path: (
            len(path.relative_to(prefix).parts),
            path.relative_to(prefix).as_posix(),
        ),
    )
    if not matches:
        raise ReleaseNoticeError(f"FFmpeg release input is missing {filename}")
    return matches[0]


def ffmpeg_inventory(
    repository: Path,
    prefix_argument: Path,
    source_argument: str,
    output: Path,
) -> dict[str, Any]:
    prefix = prefix_argument.resolve(strict=True)
    if not prefix.is_dir():
        raise ReleaseNoticeError("FFmpeg prefix is not a directory")
    source_root, source_provenance = ffmpeg_source_provenance(
        repository, prefix, source_argument
    )
    binary_candidates = (prefix / "bin/ffmpeg", prefix / "ffmpeg")
    ffmpeg = next(
        (
            path
            for path in binary_candidates
            if path.is_file() and os.access(path, os.X_OK)
        ),
        None,
    )
    if ffmpeg is None:
        raise ReleaseNoticeError("FFmpeg prefix does not contain an executable ffmpeg")

    result = subprocess.run(
        [str(ffmpeg), "-version"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseNoticeError("FFmpeg could not report its release configuration")
    lowered = result.stdout.lower()
    if "--enable-gpl" in lowered or "--enable-nonfree" in lowered:
        raise ReleaseNoticeError("FFmpeg release input is not LGPL-compatible")
    lines = result.stdout.splitlines()
    version_line = next((line.strip() for line in lines if line.strip()), "")
    configuration_line = next(
        (
            line.split("configuration:", 1)[1].strip()
            for line in lines
            if "configuration:" in line
        ),
        None,
    )
    if not version_line or configuration_line is None:
        raise ReleaseNoticeError(
            "FFmpeg version output is missing version or configuration metadata"
        )
    configuration = sanitize_configuration(configuration_line)

    ffmpeg_output = output / "ffmpeg"
    legal_root = source_root or prefix
    license_md = find_ffmpeg_input(legal_root, "LICENSE.md")
    lgpl_candidates = sorted(
        (path for path in legal_root.rglob("COPYING.LGPL*") if path.is_file()),
        key=lambda path: (
            len(path.relative_to(legal_root).parts),
            path.relative_to(legal_root).as_posix(),
        ),
    )
    if not lgpl_candidates:
        raise ReleaseNoticeError("FFmpeg release input is missing an LGPL license file")

    copied: list[str] = []
    used_names: dict[str, str] = {}
    for source in unique_paths([license_md, *lgpl_candidates]):
        basename = safe_filename(source.name)
        digest = sha256_file(source)
        prior_digest = used_names.get(basename.lower())
        if prior_digest == digest:
            continue
        if prior_digest is not None:
            stem, suffix = os.path.splitext(basename)
            basename = f"{stem}-{digest[:12]}{suffix}"
        used_names[basename.lower()] = digest
        destination = ffmpeg_output / basename
        copy_file(source, destination)
        copied.append(destination.relative_to(output).as_posix())
    copied.sort()

    write_text(ffmpeg_output / "configuration.txt", configuration)
    provenance = {
        "configuration_file": "ffmpeg/configuration.txt",
        "license_files": copied,
        "version_line": version_line,
        **source_provenance,
    }
    assert_safe_generated_metadata(
        {"configuration": configuration, **provenance},
        (repository, prefix, source_root or prefix, Path.home()),
    )
    write_json(ffmpeg_output / "provenance.json", provenance)
    return provenance


def copy_project_inputs(repository: Path, output: Path) -> list[dict[str, str]]:
    for filename in PROJECT_NOTICE_FILES:
        copy_file(repository / filename, output / filename)

    model_output = output / "models"
    models: list[dict[str, str]] = []
    for relative in MODEL_MANIFEST_FILES:
        source = repository / relative
        destination = model_output / source.name
        copy_file(source, destination)
        models.append(
            {
                "path": destination.relative_to(output).as_posix(),
                "sha256": sha256_file(destination),
            }
        )
    return models


def normalize_timestamps(root: Path) -> None:
    paths = sorted(root.rglob("*"), key=lambda path: path.as_posix(), reverse=True)
    for path in [*paths, root]:
        if not path.is_symlink():
            os.utime(path, (0, 0), follow_symlinks=False)


def generate_bundle(
    repository: Path,
    output: Path,
    ffmpeg_source: str,
    ffmpeg_prefix: Path,
    python: Path,
) -> None:
    models = copy_project_inputs(repository, output)
    ffmpeg = ffmpeg_inventory(repository, ffmpeg_prefix, ffmpeg_source, output)
    dependencies = {
        "node": node_inventory(repository, output),
        "python": python_inventory(python, output, repository=repository),
        "rust": rust_inventory(repository, output),
    }
    inventory = {
        "schema_version": 1,
        "product": "SayTrace",
        "project_notice_files": list(PROJECT_NOTICE_FILES),
        "model_manifests": models,
        "ffmpeg": ffmpeg,
        "dependencies": dependencies,
    }
    assert_safe_generated_metadata(
        inventory,
        (repository, ffmpeg_prefix, python, Path.home()),
    )
    write_json(output / "dependency-inventory.json", inventory)
    normalize_timestamps(output)


def replace_output(
    repository: Path,
    output: Path,
    ffmpeg_source: str,
    ffmpeg_prefix: Path,
    python: Path,
) -> None:
    temporary_root = Path(
        tempfile.mkdtemp(prefix=".release-notices-", dir=output.parent)
    )
    staged_output = temporary_root / "payload"
    staged_output.mkdir(mode=0o755)
    try:
        generate_bundle(repository, staged_output, ffmpeg_source, ffmpeg_prefix, python)
        if output.exists():
            if output.is_symlink() or not output.is_dir():
                raise ReleaseNoticeError(
                    "Existing release-notice output is unsafe to replace"
                )
            shutil.rmtree(output)
        os.replace(staged_output, output)
    finally:
        shutil.rmtree(temporary_root, ignore_errors=True)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository-root", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--ffmpeg-source", required=True)
    parser.add_argument("--ffmpeg-prefix", required=True, type=Path)
    parser.add_argument("--python", type=Path)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        repository = validate_repository(args.repository_root)
        output = validate_output_path(repository, args.output)
        python = args.python or repository / "worker/.venv/bin/python"
        if not python.is_absolute():
            python = repository / python
        replace_output(
            repository, output, args.ffmpeg_source, args.ffmpeg_prefix, python
        )
    except (OSError, ReleaseNoticeError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    print(f"Generated release notices: {output.relative_to(repository).as_posix()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
