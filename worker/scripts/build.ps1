#requires -Version 7.2

[CmdletBinding()]
param(
    [string]$OutputDirectory = "",
    [string]$FfmpegDirectory = ""
)

$ErrorActionPreference = "Stop"
$workerRoot = Split-Path -Parent $PSScriptRoot
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $workerRoot "dist"
}
$resolvedOutput = [IO.Path]::GetFullPath($OutputDirectory)
$previousBuildFfmpegBin = $env:LOCAL_TRANSCRIPT_BUILD_FFMPEG_BIN
if ($FfmpegDirectory) {
    $resolvedFfmpegDirectory = [IO.Path]::GetFullPath($FfmpegDirectory)
    $ffmpegCandidates = @(
        (Join-Path $resolvedFfmpegDirectory "ffmpeg.exe"),
        (Join-Path $resolvedFfmpegDirectory "bin\ffmpeg.exe")
    )
    $ffmpegExecutable = $ffmpegCandidates |
        Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
        Select-Object -First 1
    if (-not $ffmpegExecutable) {
        throw "The worker build FFmpeg directory does not contain ffmpeg.exe."
    }
    $env:LOCAL_TRANSCRIPT_BUILD_FFMPEG_BIN = Split-Path -Parent $ffmpegExecutable
}

Push-Location $workerRoot
try {
    uv sync --frozen --extra ml --group build --no-dev
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to create the locked worker build environment."
    }

    uv run --frozen --extra ml --group build --no-dev pyinstaller `
        --clean `
        --noconfirm `
        --distpath $resolvedOutput `
        (Join-Path $workerRoot "local_transcript_worker.spec")
    if ($LASTEXITCODE -ne 0) {
        throw "PyInstaller failed."
    }

    $bundle = Join-Path $resolvedOutput "local-transcript-worker"
    $workerExecutable = Join-Path $bundle "local-transcript-worker.exe"
    & $workerExecutable --version
    if ($LASTEXITCODE -ne 0) {
        throw "The packaged worker could not import and start after its build."
    }

    $hashLines = Get-ChildItem -File -Recurse -LiteralPath $bundle |
        Sort-Object FullName |
        ForEach-Object {
            $relative = [IO.Path]::GetRelativePath($bundle, $_.FullName).Replace("\", "/")
            $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash.ToLowerInvariant()
            "$hash  $relative"
        }
    $hashPath = Join-Path $bundle "SHA256SUMS.txt"
    [IO.File]::WriteAllLines($hashPath, $hashLines, [Text.UTF8Encoding]::new($false))

    Write-Host "Worker bundle: $bundle"
    Write-Host "Hashes: $hashPath"
} finally {
    Pop-Location
    if ($null -eq $previousBuildFfmpegBin) {
        Remove-Item Env:\LOCAL_TRANSCRIPT_BUILD_FFMPEG_BIN -ErrorAction SilentlyContinue
    } else {
        $env:LOCAL_TRANSCRIPT_BUILD_FFMPEG_BIN = $previousBuildFfmpegBin
    }
}
