#requires -Version 7.2

[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "release-common.ps1")

$repositoryRoot = Get-ReleaseRepositoryRoot
$makeNsis = Resolve-ReleaseMakeNsis
if (-not $makeNsis) {
    throw "makensis.exe was not found. Install NSIS 3 or set LOCAL_TRANSCRIPT_MAKENSIS_PATH."
}

$smokeName = "LocalTranscriptRuntimeSmoke-$([Guid]::NewGuid().ToString('N')).exe"
$smokeOutput = Join-Path ([IO.Path]::GetTempPath()) $smokeName
try {
    $include = Join-Path $repositoryRoot "packaging\runtime\runtime-files.smoke.nsh"
    $smokeFile = Join-Path $repositoryRoot "packaging\revisions.json"
    $icon = Join-Path $repositoryRoot "src-tauri\icons\icon.ico"
    $installer = Join-Path $repositoryRoot "packaging\runtime\runtime-installer.nsi"
    Invoke-ReleaseNative `
        -FilePath $makeNsis `
        -ArgumentList @(
            "/V2",
            "/DFILES_INCLUDE=$include",
            "/DSMOKE_FILE=$smokeFile",
            "/DOUTPUT_FILE=$smokeOutput",
            "/DINSTALLER_ICON=$icon",
            "/DRUNTIME_VARIANT=Smoke",
            "/DRUNTIME_VERSION=0.1.0",
            "/DVERSION_QUAD=0.1.0.0",
            $installer
        ) `
        -FailureMessage "The runtime NSIS template did not compile."
    $item = Get-Item -LiteralPath $smokeOutput
    Write-Host "NSIS template smoke compile: OK ($($item.Length) bytes)"
} finally {
    $resolvedSmoke = [IO.Path]::GetFullPath($smokeOutput)
    $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (
        $resolvedSmoke.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path -Leaf $resolvedSmoke).StartsWith("LocalTranscriptRuntimeSmoke-", [StringComparison]::Ordinal)
    ) {
        Remove-Item -LiteralPath $resolvedSmoke -Force -ErrorAction SilentlyContinue
    }
}

$ffmpeg = Get-Command ffmpeg.exe -ErrorAction SilentlyContinue
if ($ffmpeg) {
    try {
        Get-LgplFfmpegMetadata -FfmpegPath $ffmpeg.Source | Out-Null
        Write-Host "Installed FFmpeg license gate: LGPL-compatible"
    } catch {
        if ($_.Exception.Message -match "enables GPL components|enables nonfree components") {
            Write-Host "Installed FFmpeg license gate: correctly rejected non-LGPL build"
        } else {
            throw
        }
    }
} else {
    Write-Host "Installed FFmpeg license gate: skipped (ffmpeg.exe not installed)"
}

$manifestSmokeRoot = Join-Path ([IO.Path]::GetTempPath()) "LocalTranscriptManifestSmoke-$([Guid]::NewGuid().ToString('N'))"
try {
    [IO.Directory]::CreateDirectory($manifestSmokeRoot) | Out-Null
    $artifactPath = Join-Path $manifestSmokeRoot "unsigned-smoke.exe"
    [IO.File]::WriteAllBytes($artifactPath, [byte[]](0x4C, 0x54, 0x52, 0x31))
    $record = Get-ReleaseFileRecord -FilePath $artifactPath -RelativeTo $manifestSmokeRoot
    $manifestPath = Join-Path $manifestSmokeRoot "release-manifest.json"
    Write-ReleaseJson -Path $manifestPath -Value ([ordered]@{
        schema_version = 1
        artifact_kind  = "app_installer"
        signed         = $false
        installer      = $record
    })
    & (Join-Path $PSScriptRoot "release-verify.ps1") -ManifestPath $manifestPath

    $record.path = "../outside.exe"
    Write-ReleaseJson -Path $manifestPath -Value ([ordered]@{
        schema_version = 1
        artifact_kind  = "app_installer"
        signed         = $false
        installer      = $record
    })
    try {
        & (Join-Path $PSScriptRoot "release-verify.ps1") -ManifestPath $manifestPath
        throw "Unsafe manifest smoke test unexpectedly succeeded."
    } catch {
        if ($_.Exception.Message -notmatch "unsafe payload path|escapes its payload root") {
            throw
        }
        Write-Host "Manifest traversal gate: correctly rejected unsafe path"
    }
} finally {
    $resolvedManifestSmoke = [IO.Path]::GetFullPath($manifestSmokeRoot)
    $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (
        $resolvedManifestSmoke.StartsWith($temporaryRoot, [StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path -Leaf $resolvedManifestSmoke).StartsWith("LocalTranscriptManifestSmoke-", [StringComparison]::Ordinal)
    ) {
        Remove-Item -LiteralPath $resolvedManifestSmoke -Recurse -Force -ErrorAction SilentlyContinue
    }
}
