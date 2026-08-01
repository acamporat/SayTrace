from __future__ import annotations

import hashlib
import sys
import types
from pathlib import Path

from local_transcript_worker.errors import ErrorCode
from local_transcript_worker.models import (
    FileHash,
    ModelFile,
    ModelManifest,
    ModelSpec,
    ModelStore,
    _download_error,
)


def test_shipped_manifest_uses_revision_pins_and_sha256_only() -> None:
    manifest = ModelManifest.load()

    assert manifest.models
    assert all(len(model.revision) == 40 for model in manifest.models)
    assert all(
        item.digest.algorithm == "sha256" and len(item.digest.value) == 64
        for model in manifest.models
        for item in model.files
    )


def test_store_detects_hash_mismatch(tmp_path: Path) -> None:
    content = b"expected bytes"
    digest = hashlib.sha256(content).hexdigest()
    spec = ModelSpec(
        key="fake",
        purpose="test",
        repository="example/fake",
        revision="a" * 40,
        license="test",
        gated=False,
        files=(ModelFile("model.bin", len(content), FileHash("sha256", digest)),),
    )
    manifest = ModelManifest(1, "test-pipeline", (spec,))
    store = ModelStore(tmp_path, manifest)
    model_path = store.model_path("fake")
    model_path.mkdir(parents=True)
    (model_path / "model.bin").write_bytes(content)

    assert store.verify("fake")["ready"] is True

    (model_path / "model.bin").write_bytes(b"tampered bytes")
    status = store.verify("fake")

    assert status["ready"] is False
    assert status["failures"][0]["reason"] in {"size", "hash"}


def test_install_quarantines_invalid_target_until_verified_publish(
    tmp_path: Path, monkeypatch
) -> None:
    content = b"verified model"
    spec = ModelSpec(
        key="fake",
        purpose="test",
        repository="example/fake",
        revision="a" * 40,
        license="test",
        gated=False,
        files=(
            ModelFile(
                "model.bin",
                len(content),
                FileHash("sha256", hashlib.sha256(content).hexdigest()),
            ),
        ),
    )
    store = ModelStore(tmp_path, ModelManifest(1, "pipeline", (spec,)), setup_enabled=True)
    target = store.model_path("fake")
    target.mkdir(parents=True)
    (target / "model.bin").write_bytes(b"corrupt")

    fake_hub = types.ModuleType("huggingface_hub")

    def snapshot_download(**kwargs) -> None:
        staging = Path(kwargs["local_dir"])
        staging.mkdir(parents=True)
        (staging / "model.bin").write_bytes(content)

    fake_hub.snapshot_download = snapshot_download  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "huggingface_hub", fake_hub)
    progress: list[tuple[str, int, int]] = []

    result = store.install(
        "fake", progress=lambda phase, completed, total: progress.append((phase, completed, total))
    )

    assert result["ready"] is True
    assert (target / "model.bin").read_bytes() == content
    assert not list(target.parent.glob(".corrupt-*"))
    assert progress == [
        ("checking", 0, 4),
        ("downloading", 1, 4),
        ("verifying", 2, 4),
        ("publishing", 3, 4),
        ("complete", 4, 4),
    ]


def test_gated_download_error_explains_same_account_access() -> None:
    spec = ModelSpec(
        key="diarization",
        purpose="test",
        repository="pyannote/speaker-diarization-community-1",
        revision="a" * 40,
        license="CC-BY-4.0",
        gated=True,
        files=(),
    )

    class Response:
        status_code = 401

    class HubFailure(Exception):
        response = Response()

    error = _download_error(spec, HubFailure())

    assert error.code is ErrorCode.MODEL_ACCESS_DENIED
    assert error.retryable is True
    assert "same account" not in error.message
    assert "account that created this token" in error.message
    assert error.details == {
        "model": "diarization",
        "repository": "pyannote/speaker-diarization-community-1",
        "exception": "HubFailure",
        "http_status": 401,
    }


def test_missing_pinned_download_requires_an_app_update() -> None:
    spec = ModelSpec(
        key="fake",
        purpose="test",
        repository="example/fake",
        revision="a" * 40,
        license="test",
        gated=False,
        files=(),
    )

    class Response:
        status_code = 404

    class HubFailure(Exception):
        response = Response()

    error = _download_error(spec, HubFailure())

    assert error.code is ErrorCode.MODEL_MANIFEST_STALE
    assert error.retryable is False
    assert "updated SayTrace release" in error.message
