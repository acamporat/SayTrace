#requires -Version 7.2

Set-StrictMode -Version Latest

function Get-ReleaseRepositoryRoot {
    return [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
}

function Write-ReleaseJson {
    param(
        [Parameter(Mandatory)]
        [object]$Value,

        [Parameter(Mandatory)]
        [string]$Path
    )

    $parent = Split-Path -Parent $Path
    if ($parent) {
        [IO.Directory]::CreateDirectory($parent) | Out-Null
    }
    $json = $Value | ConvertTo-Json -Depth 100
    [IO.File]::WriteAllText(
        [IO.Path]::GetFullPath($Path),
        "$json`n",
        [Text.UTF8Encoding]::new($false)
    )
}

function Invoke-ReleaseNative {
    param(
        [Parameter(Mandatory)]
        [string]$FilePath,

        [Parameter()]
        [string[]]$ArgumentList = @(),

        [Parameter()]
        [string]$FailureMessage = "Command failed."
    )

    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$FailureMessage Exit code: $LASTEXITCODE"
    }
}

function Get-ReleaseRevision {
    param(
        [Parameter(Mandatory)]
        [string]$RepositoryRoot
    )

    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if (-not $git) {
        return "unavailable"
    }
    $revision = & $git.Source -C $RepositoryRoot rev-parse --verify HEAD 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $revision) {
        return "uncommitted"
    }
    return $revision.Trim()
}

function Test-ReleaseWorkingTreeDirty {
    param(
        [Parameter(Mandatory)]
        [string]$RepositoryRoot
    )

    $git = Get-Command git.exe -ErrorAction SilentlyContinue
    if (-not $git) {
        return $true
    }
    $status = & $git.Source -C $RepositoryRoot status --porcelain 2>$null
    return $LASTEXITCODE -ne 0 -or [bool]$status
}

function Get-ReleaseFileRecord {
    param(
        [Parameter(Mandatory)]
        [string]$FilePath,

        [Parameter(Mandatory)]
        [string]$RelativeTo
    )

    $resolvedFile = [IO.Path]::GetFullPath($FilePath)
    $resolvedRoot = [IO.Path]::GetFullPath($RelativeTo)
    $relative = [IO.Path]::GetRelativePath($resolvedRoot, $resolvedFile).Replace("\", "/")
    if ($relative -eq ".." -or $relative.StartsWith("../", [StringComparison]::Ordinal)) {
        throw "File is outside the declared payload root: $resolvedFile"
    }
    $item = Get-Item -LiteralPath $resolvedFile -ErrorAction Stop
    return [ordered]@{
        path   = $relative
        size   = $item.Length
        sha256 = (Get-FileHash -LiteralPath $resolvedFile -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}

function Resolve-ReleaseSignTool {
    param(
        [Parameter()]
        [string]$RequestedPath = ""
    )

    $candidates = [Collections.Generic.List[string]]::new()
    if ($RequestedPath) {
        $candidates.Add($RequestedPath)
    }
    if ($env:LOCAL_TRANSCRIPT_SIGNTOOL_PATH) {
        $candidates.Add($env:LOCAL_TRANSCRIPT_SIGNTOOL_PATH)
    }
    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($command) {
        $candidates.Add($command.Source)
    }

    $kitsRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    if (Test-Path -LiteralPath $kitsRoot -PathType Container) {
        Get-ChildItem -LiteralPath $kitsRoot -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object {
                $candidates.Add((Join-Path $_.FullName "x64\signtool.exe"))
            }
    }

    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            return [IO.Path]::GetFullPath($candidate)
        }
    }
    return $null
}

function Get-ReleaseSigningCertificate {
    param(
        [Parameter(Mandatory)]
        [string]$Thumbprint
    )

    if (-not $IsWindows) {
        throw "Authenticode release signing is supported only on Windows."
    }
    $normalized = ($Thumbprint -replace "[^0-9A-Fa-f]", "").ToUpperInvariant()
    if ($normalized.Length -ne 40) {
        throw "The signing certificate thumbprint must contain exactly 40 hexadecimal characters."
    }
    $certificate = Get-Item -LiteralPath "Cert:\CurrentUser\My\$normalized" -ErrorAction SilentlyContinue
    if (-not $certificate) {
        throw "No certificate with thumbprint $normalized exists in Cert:\CurrentUser\My."
    }
    if (-not $certificate.HasPrivateKey) {
        throw "Certificate $normalized does not have an accessible private key."
    }
    $codeSigningOid = "1.3.6.1.5.5.7.3.3"
    $enhancedUses = @($certificate.EnhancedKeyUsageList | ForEach-Object { $_.ObjectId.Value })
    if ($codeSigningOid -notin $enhancedUses) {
        throw "Certificate $normalized is not valid for code signing."
    }
    $now = Get-Date
    if ($certificate.NotBefore -gt $now -or $certificate.NotAfter -le $now) {
        throw "Certificate $normalized is not currently valid."
    }
    return $certificate
}

function Assert-ReleaseSigningReady {
    param(
        [Parameter()]
        [string]$SignToolPath = "",

        [Parameter(Mandatory)]
        [string]$CertificateThumbprint
    )

    $tool = Resolve-ReleaseSignTool -RequestedPath $SignToolPath
    if (-not $tool) {
        throw "SignTool was not found. Install the Windows SDK or set LOCAL_TRANSCRIPT_SIGNTOOL_PATH."
    }
    $certificate = Get-ReleaseSigningCertificate -Thumbprint $CertificateThumbprint
    return [ordered]@{
        signToolPath = $tool
        thumbprint   = ($certificate.Thumbprint -replace " ", "").ToUpperInvariant()
        subject      = $certificate.Subject
    }
}

function Invoke-ReleaseSigning {
    param(
        [Parameter(Mandatory)]
        [string]$FilePath,

        [Parameter(Mandatory)]
        [string]$SignToolPath,

        [Parameter(Mandatory)]
        [string]$CertificateThumbprint,

        [Parameter()]
        [string]$TimestampUrl = "http://timestamp.digicert.com"
    )

    $resolvedFile = [IO.Path]::GetFullPath($FilePath)
    if (-not (Test-Path -LiteralPath $resolvedFile -PathType Leaf)) {
        throw "Cannot sign missing file: $resolvedFile"
    }
    $ready = Assert-ReleaseSigningReady `
        -SignToolPath $SignToolPath `
        -CertificateThumbprint $CertificateThumbprint
    $arguments = @(
        "sign",
        "/fd", "SHA256",
        "/sha1", $ready.thumbprint,
        "/s", "My"
    )
    if ($TimestampUrl) {
        $arguments += @("/tr", $TimestampUrl, "/td", "SHA256")
    }
    $arguments += $resolvedFile
    Invoke-ReleaseNative `
        -FilePath $ready.signToolPath `
        -ArgumentList $arguments `
        -FailureMessage "SignTool could not sign $resolvedFile."
}

function Resolve-ReleaseMakeNsis {
    param(
        [Parameter()]
        [string]$RequestedPath = ""
    )

    $candidates = [Collections.Generic.List[string]]::new()
    if ($RequestedPath) {
        $candidates.Add($RequestedPath)
    }
    if ($env:LOCAL_TRANSCRIPT_MAKENSIS_PATH) {
        $candidates.Add($env:LOCAL_TRANSCRIPT_MAKENSIS_PATH)
    }
    $command = Get-Command makensis.exe -ErrorAction SilentlyContinue
    if ($command) {
        $candidates.Add($command.Source)
    }
    if (${env:ProgramFiles(x86)}) {
        $candidates.Add((Join-Path ${env:ProgramFiles(x86)} "NSIS\makensis.exe"))
    }
    if ($env:ProgramFiles) {
        $candidates.Add((Join-Path $env:ProgramFiles "NSIS\makensis.exe"))
    }
    if ($env:LOCALAPPDATA) {
        $candidates.Add((Join-Path $env:LOCALAPPDATA "tauri\NSIS\makensis.exe"))
    }

    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            return [IO.Path]::GetFullPath($candidate)
        }
    }
    return $null
}

function Get-LgplFfmpegMetadata {
    param(
        [Parameter(Mandatory)]
        [string]$FfmpegPath
    )

    $resolved = [IO.Path]::GetFullPath($FfmpegPath)
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
        throw "FFmpeg executable does not exist: $resolved"
    }
    $versionOutput = (& $resolved -hide_banner -version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "FFmpeg could not report its version."
    }
    $licenseOutput = (& $resolved -L 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "FFmpeg could not report its license configuration."
    }
    $combined = "$versionOutput`n$licenseOutput"
    if ($combined -match "(?i)--enable-nonfree") {
        throw "The FFmpeg build enables nonfree components and cannot be packaged."
    }
    if ($combined -match "(?i)--enable-gpl") {
        throw "The FFmpeg build enables GPL components. Supply an LGPL-compatible build."
    }
    if ($combined -notmatch "(?i)lesser general public license|\bLGPL\b") {
        throw "FFmpeg did not identify itself as an LGPL build."
    }
    $firstLine = ($versionOutput -split "\r?\n")[0].Trim()
    return [ordered]@{
        versionLine = $firstLine
        license     = if ($combined -match "(?i)LGPL version 3") { "LGPL-3.0-or-later" } else { "LGPL-2.1-or-later" }
    }
}
