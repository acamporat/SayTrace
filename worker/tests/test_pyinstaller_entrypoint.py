from __future__ import annotations

import importlib.util
import os
import stat
from pathlib import Path
from types import ModuleType

import pytest

ENTRYPOINT = Path(__file__).parents[1] / "pyinstaller_entrypoint.py"


def load_entrypoint() -> ModuleType:
    specification = importlib.util.spec_from_file_location(
        "saytrace_pyinstaller_entrypoint",
        ENTRYPOINT,
    )
    assert specification is not None and specification.loader is not None
    module = importlib.util.module_from_spec(specification)
    specification.loader.exec_module(module)
    return module


def test_macos_frozen_worker_uses_private_path_keyed_font_cache(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    entrypoint = load_entrypoint()
    runtime = tmp_path / "runtime" / "_internal"
    runtime.mkdir(parents=True)
    monkeypatch.setattr(entrypoint.sys, "platform", "darwin")
    monkeypatch.setattr(entrypoint.sys, "frozen", True, raising=False)
    monkeypatch.delenv("MPLCONFIGDIR", raising=False)

    selected = entrypoint.configure_macos_matplotlib_cache(
        home=tmp_path / "home",
        runtime_root=runtime,
    )

    assert selected is not None
    assert selected.is_dir()
    assert selected.parent.name == "matplotlib"
    assert len(selected.name) == 16
    assert stat.S_IMODE(selected.stat().st_mode) == 0o700
    assert os.environ["MPLCONFIGDIR"] == str(selected)
    assert (
        entrypoint.configure_macos_matplotlib_cache(
            home=tmp_path / "home",
            runtime_root=runtime,
        )
        == selected
    )


def test_unfrozen_worker_preserves_existing_matplotlib_configuration(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    entrypoint = load_entrypoint()
    monkeypatch.setattr(entrypoint.sys, "platform", "darwin")
    monkeypatch.setattr(entrypoint.sys, "frozen", False, raising=False)
    monkeypatch.setenv("MPLCONFIGDIR", "host-owned-cache")

    assert (
        entrypoint.configure_macos_matplotlib_cache(
            home=tmp_path,
            runtime_root=tmp_path,
        )
        is None
    )
    assert os.environ["MPLCONFIGDIR"] == "host-owned-cache"
