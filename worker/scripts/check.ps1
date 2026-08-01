$ErrorActionPreference = "Stop"
$workerRoot = Split-Path -Parent $PSScriptRoot

Push-Location $workerRoot
try {
    uv lock --check
    if ($LASTEXITCODE -ne 0) { throw "uv.lock is stale." }
    uv run --frozen ruff check .
    if ($LASTEXITCODE -ne 0) { throw "Ruff failed." }
    uv run --frozen ruff format --check .
    if ($LASTEXITCODE -ne 0) { throw "Ruff formatting check failed." }
    uv run --frozen mypy
    if ($LASTEXITCODE -ne 0) { throw "mypy failed." }
    uv run --frozen pytest
    if ($LASTEXITCODE -ne 0) { throw "pytest failed." }
} finally {
    Pop-Location
}
