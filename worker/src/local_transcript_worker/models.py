"""Revision-pinned model pack verification and one-time setup."""

from __future__ import annotations

import hashlib
import json
import re
import shutil
import uuid
from collections.abc import Callable
from dataclasses import dataclass
from importlib import resources
from pathlib import Path
from typing import Any

from .errors import ErrorCode, WorkerError, missing_dependency

_SAFE_KEY = re.compile(r"^[a-z][a-z0-9_]{0,63}$")
_SAFE_REPOSITORY = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
_HEX = frozenset("0123456789abcdef")
ModelProgress = Callable[[str, int, int], None]


@dataclass(frozen=True, slots=True)
class FileHash:
    algorithm: str
    value: str


@dataclass(frozen=True, slots=True)
class ModelFile:
    path: str
    size: int
    digest: FileHash


@dataclass(frozen=True, slots=True)
class ModelSpec:
    key: str
    purpose: str
    repository: str
    revision: str
    license: str
    gated: bool
    files: tuple[ModelFile, ...]


@dataclass(frozen=True, slots=True)
class ModelManifest:
    schema_version: int
    pipeline_version: str
    models: tuple[ModelSpec, ...]

    @classmethod
    def load(cls, path: Path | None = None) -> ModelManifest:
        if path is None:
            packaged = resources.files("local_transcript_worker").joinpath("model-manifest.json")
            if packaged.is_file():
                raw = packaged.read_text(encoding="utf-8")
            else:
                source_root = Path(__file__).resolve().parents[2]
                raw = (source_root / "model-manifest.json").read_text(encoding="utf-8")
        else:
            raw = path.read_text(encoding="utf-8")
        try:
            data: Any = json.loads(raw)
            models = tuple(_parse_model(value) for value in data["models"])
            manifest = cls(
                schema_version=int(data["schema_version"]),
                pipeline_version=str(data["pipeline_version"]),
                models=models,
            )
        except (KeyError, TypeError, ValueError, json.JSONDecodeError) as exc:
            raise WorkerError(ErrorCode.MODEL_SETUP_FAILED, "Invalid model manifest.") from exc
        manifest.validate()
        return manifest

    def validate(self) -> None:
        if self.schema_version != 1 or not self.models:
            raise WorkerError(ErrorCode.MODEL_SETUP_FAILED, "Unsupported or empty model manifest.")
        keys: set[str] = set()
        for model in self.models:
            if (
                model.key in keys
                or not _SAFE_KEY.fullmatch(model.key)
                or not _SAFE_REPOSITORY.fullmatch(model.repository)
                or len(model.revision) != 40
                or any(character not in _HEX for character in model.revision)
            ):
                raise WorkerError(
                    ErrorCode.MODEL_SETUP_FAILED, "Invalid model key or revision pin."
                )
            keys.add(model.key)
            for item in model.files:
                path = Path(item.path)
                if path.is_absolute() or ".." in path.parts or item.size <= 0:
                    raise WorkerError(ErrorCode.MODEL_SETUP_FAILED, "Unsafe model manifest path.")
                expected_length = 64
                if item.digest.algorithm != "sha256":
                    raise WorkerError(ErrorCode.MODEL_SETUP_FAILED, "Unsupported model hash.")
                if len(item.digest.value) != expected_length or any(
                    character not in _HEX for character in item.digest.value
                ):
                    raise WorkerError(ErrorCode.MODEL_SETUP_FAILED, "Malformed model hash.")

    def by_key(self, key: str) -> ModelSpec:
        for model in self.models:
            if model.key == key:
                return model
        raise WorkerError(ErrorCode.BAD_REQUEST, f"Unknown model key {key!r}.")


def _parse_model(data: dict[str, Any]) -> ModelSpec:
    return ModelSpec(
        key=str(data["key"]),
        purpose=str(data["purpose"]),
        repository=str(data["repository"]),
        revision=str(data["revision"]),
        license=str(data["license"]),
        gated=bool(data["gated"]),
        files=tuple(
            ModelFile(
                path=str(item["path"]),
                size=int(item["size"]),
                digest=FileHash(
                    algorithm=str(item["hash"]["algorithm"]),
                    value=str(item["hash"]["value"]).lower(),
                ),
            )
            for item in data["files"]
        ),
    )


def _digest(path: Path, algorithm: str) -> str:
    if algorithm == "sha256":
        digest = hashlib.sha256()
    else:
        raise WorkerError(ErrorCode.MODEL_SETUP_FAILED, f"Unsupported hash {algorithm!r}.")
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _download_error(spec: ModelSpec, exc: Exception) -> WorkerError:
    """Translate Hub failures without persisting tokens or remote response bodies."""

    response = getattr(exc, "response", None)
    status_code = getattr(response, "status_code", None)
    exception_name = type(exc).__name__
    details: dict[str, Any] = {
        "model": spec.key,
        "repository": spec.repository,
        "exception": exception_name,
    }
    if isinstance(status_code, int):
        details["http_status"] = status_code

    if status_code in {401, 403} or exception_name in {
        "GatedRepoError",
        "LocalTokenNotFoundError",
        "XetAuthorizationError",
    }:
        return WorkerError(
            ErrorCode.MODEL_ACCESS_DENIED,
            (
                "Hugging Face denied access to Community-1. Sign in with the "
                "account that created this token, accept the Community-1 "
                "conditions, then retry. If access is already granted, create "
                "a new read token from that account."
            ),
            details,
            retryable=True,
        )

    if status_code in {404, 410} or exception_name in {
        "EntryNotFoundError",
        "RepositoryNotFoundError",
        "RevisionNotFoundError",
    }:
        return WorkerError(
            ErrorCode.MODEL_MANIFEST_STALE,
            (
                f"The pinned files for model pack {spec.key!r} are no longer "
                "available. Install an updated SayTrace release."
            ),
            details,
        )

    return WorkerError(
        ErrorCode.MODEL_DOWNLOAD_FAILED,
        (
            f"Could not download model pack {spec.key!r}. Check this PC's "
            "internet connection and retry."
        ),
        details,
        retryable=True,
    )


class ModelStore:
    def __init__(self, root: Path, manifest: ModelManifest, *, setup_enabled: bool = False) -> None:
        self.root = root.resolve(strict=False)
        self.manifest = manifest
        self.setup_enabled = setup_enabled

    def model_path(self, key: str) -> Path:
        spec = self.manifest.by_key(key)
        return self.root / spec.key / spec.revision

    def verify(
        self, key: str, *, verify_hashes: bool = True, location: Path | None = None
    ) -> dict[str, Any]:
        spec = self.manifest.by_key(key)
        location = location or self.model_path(key)
        resolved_location = location.resolve(strict=False)
        failures: list[dict[str, Any]] = []
        total_bytes = 0
        for expected in spec.files:
            path = location / Path(expected.path)
            if not path.is_file():
                failures.append({"path": expected.path, "reason": "missing"})
                continue
            resolved_path = path.resolve(strict=True)
            if (
                resolved_path != resolved_location
                and resolved_location not in resolved_path.parents
            ):
                failures.append({"path": expected.path, "reason": "path_escape"})
                continue
            actual_size = path.stat().st_size
            total_bytes += actual_size
            if actual_size != expected.size:
                failures.append(
                    {
                        "path": expected.path,
                        "reason": "size",
                        "expected": expected.size,
                        "actual": actual_size,
                    }
                )
                continue
            actual = _digest(path, expected.digest.algorithm) if verify_hashes else None
            if verify_hashes and actual != expected.digest.value:
                failures.append(
                    {
                        "path": expected.path,
                        "reason": "hash",
                        "algorithm": expected.digest.algorithm,
                        "expected": expected.digest.value,
                        "actual": actual,
                    }
                )
        return {
            "key": key,
            "revision": spec.revision,
            "path": str(location),
            "ready": not failures,
            "verified_bytes": total_bytes,
            "failures": failures,
        }

    def status(self, *, verify_hashes: bool = True) -> dict[str, Any]:
        return {
            "pipeline_version": self.manifest.pipeline_version,
            "models": [
                self.verify(model.key, verify_hashes=verify_hashes)
                for model in self.manifest.models
            ],
        }

    def require(self, key: str) -> Path:
        result = self.verify(key)
        if not result["ready"]:
            raise WorkerError(
                ErrorCode.MODEL_MISSING,
                f"Model pack {key!r} is missing or corrupt.",
                result,
            )
        return self.model_path(key)

    def install(
        self,
        key: str,
        *,
        token: str | None = None,
        progress: ModelProgress | None = None,
    ) -> dict[str, Any]:
        """Download into a staging directory, verify, then atomically publish it."""

        def report(phase: str, completed: int) -> None:
            if progress is not None:
                progress(phase, completed, 4)

        if not self.setup_enabled:
            raise WorkerError(
                ErrorCode.MODEL_INSTALL_DISABLED,
                "This worker was launched in offline inference mode.",
            )
        spec = self.manifest.by_key(key)
        if spec.gated and not token:
            raise WorkerError(
                ErrorCode.MODEL_SETUP_FAILED,
                "This model requires a one-time Hugging Face access token.",
                {"model": key, "gated": True},
            )
        try:
            from huggingface_hub import snapshot_download
        except ImportError as exc:
            raise missing_dependency("huggingface-hub", "model setup") from exc

        target = self.model_path(key)
        report("checking", 0)
        if target.exists() and self.verify(key)["ready"]:
            result = self.verify(key)
            report("complete", 4)
            return result
        target.parent.mkdir(parents=True, exist_ok=True)
        staging = target.parent / f".partial-{target.name}-{uuid.uuid4().hex}"
        quarantine: Path | None = None
        try:
            report("downloading", 1)
            snapshot_download(
                repo_id=spec.repository,
                revision=spec.revision,
                allow_patterns=[item.path for item in spec.files],
                local_dir=staging,
                token=token,
            )
            report("verifying", 2)
            verification = self.verify(key, location=staging)
            if not verification["ready"]:
                raise WorkerError(
                    ErrorCode.HASH_MISMATCH,
                    "Downloaded model pack failed integrity verification.",
                    verification,
                )
            if target.exists():
                quarantine = target.parent / f".corrupt-{target.name}-{uuid.uuid4().hex}"
                target.replace(quarantine)
            report("publishing", 3)
            staging.replace(target)
            if quarantine is not None:
                shutil.rmtree(quarantine, ignore_errors=True)
                quarantine = None
            result = self.verify(key)
            report("complete", 4)
            return result
        except WorkerError:
            raise
        except Exception as exc:
            raise _download_error(spec, exc) from exc
        finally:
            token = None
            if quarantine is not None and quarantine.exists() and not target.exists():
                quarantine.replace(target)
            if staging.exists():
                shutil.rmtree(staging, ignore_errors=True)
