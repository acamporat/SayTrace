#requires -Version 5.1

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$SourceRuntime,

    [Parameter(Mandatory)]
    [string]$DestinationRuntime,

    [Parameter(Mandatory)]
    [string]$InstallRuntimeScript,

    [Parameter(Mandatory)]
    [string]$VerifyWorkerScript,

    [Parameter(Mandatory)]
    [string]$DependencyManifest,

    [Parameter()]
    [switch]$ValidateOnly
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

function Get-SayTraceSha256 {
    param([Parameter(Mandatory)][string]$FilePath)

    return (Get-FileHash -LiteralPath $FilePath -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-SayTraceOsBuild {
    try {
        return [int](Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction Stop).BuildNumber
    } catch {
        return [int][Environment]::OSVersion.Version.Build
    }
}

function Get-SayTraceOllamaStatus {
    try {
        $version = Invoke-RestMethod `
            -UseBasicParsing `
            -Uri "http://127.0.0.1:11434/api/version" `
            -TimeoutSec 4
        $tags = Invoke-RestMethod `
            -UseBasicParsing `
            -Uri "http://127.0.0.1:11434/api/tags" `
            -TimeoutSec 8
        $localModels = @(
            $tags.models | Where-Object {
                [long]$_.size -gt 0 -and
                -not ([string]$_.name).ToLowerInvariant().Contains(":cloud")
            }
        )
        return [pscustomobject]@{
            available = $true
            version = [string]$version.version
            models = $localModels
        }
    } catch {
        return [pscustomobject]@{
            available = $false
            version = ""
            models = @()
        }
    }
}

function Wait-SayTraceOllama {
    param([int]$Seconds = 90)

    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        $status = Get-SayTraceOllamaStatus
        if ($status.available) {
            return $status
        }
        Start-Sleep -Milliseconds 750
    } while ([DateTime]::UtcNow -lt $deadline)
    return $status
}

function Resolve-SayTraceOllamaExecutable {
    $candidates = @(
        (Join-Path $env:LOCALAPPDATA "Programs\Ollama\ollama.exe"),
        (Join-Path $env:LOCALAPPDATA "Ollama\ollama.exe")
    )
    $command = Get-Command ollama.exe -ErrorAction SilentlyContinue
    if ($command) {
        $candidates += $command.Source
    }
    return $candidates |
        Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) } |
        Select-Object -First 1
}

function Start-SayTraceOllama {
    param([Parameter(Mandatory)][string]$OllamaExecutable)

    $appExecutable = Join-Path (Split-Path -Parent $OllamaExecutable) "ollama app.exe"
    if (Test-Path -LiteralPath $appExecutable -PathType Leaf) {
        Start-Process -FilePath $appExecutable -WindowStyle Hidden | Out-Null
    } else {
        Start-Process `
            -FilePath $OllamaExecutable `
            -ArgumentList "serve" `
            -WindowStyle Hidden | Out-Null
    }
}

function Install-SayTraceOllama {
    param([Parameter(Mandatory)][object]$Configuration)

    $temporary = Join-Path ([IO.Path]::GetTempPath()) "SayTrace-OllamaSetup-$PID.exe"
    try {
        Write-Host "Downloading the pinned Ollama $($Configuration.installer_version) dependency..."
        Invoke-WebRequest `
            -UseBasicParsing `
            -Uri ([string]$Configuration.installer_url) `
            -OutFile $temporary
        $actualHash = Get-SayTraceSha256 -FilePath $temporary
        if ($actualHash -ne ([string]$Configuration.installer_sha256).ToLowerInvariant()) {
            throw "The Ollama installer failed its pinned SHA-256 check."
        }
        $signature = Get-AuthenticodeSignature -LiteralPath $temporary
        if ($signature.Status -ne "Valid") {
            throw "The Ollama installer does not have a valid Authenticode signature."
        }
        Write-Host "Installing Ollama for the current Windows user..."
        $process = Start-Process `
            -FilePath $temporary `
            -ArgumentList "/VERYSILENT /NORESTART /SUPPRESSMSGBOXES" `
            -PassThru
        $process.WaitForExit()
        if ($process.ExitCode -ne 0) {
            throw "The Ollama installer exited with code $($process.ExitCode)."
        }
    } finally {
        if (Test-Path -LiteralPath $temporary -PathType Leaf) {
            Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        }
    }
}

function Install-SayTraceDefaultModel {
    param(
        [Parameter(Mandatory)][string]$OllamaExecutable,
        [Parameter(Mandatory)][object]$Configuration
    )

    Write-Host "Verifying the pinned $($Configuration.name) model manifest..."
    $remoteManifest = Invoke-WebRequest `
        -UseBasicParsing `
        -Uri ([string]$Configuration.manifest_url) `
        -TimeoutSec 60
    $algorithm = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes([string]$remoteManifest.Content)
        $actual = ([BitConverter]::ToString($algorithm.ComputeHash($bytes))).Replace("-", "").ToLowerInvariant()
    } finally {
        $algorithm.Dispose()
    }
    if ($actual -ne ([string]$Configuration.manifest_sha256).ToLowerInvariant()) {
        throw "The default agent model tag changed after this SayTrace release. Setup will not install unpinned model bytes."
    }

    Write-Host "Installing the private local agent model $($Configuration.name) (about 2.5 GB)..."
    $process = Start-Process `
        -FilePath $OllamaExecutable `
        -ArgumentList @("pull", [string]$Configuration.name) `
        -NoNewWindow `
        -PassThru `
        -Wait
    if ($process.ExitCode -ne 0) {
        throw "Ollama could not install the default local agent model."
    }
    $status = Get-SayTraceOllamaStatus
    $installed = @(
        $status.models | Where-Object {
            [string]$_.name -eq [string]$Configuration.name -and
            ([string]$_.digest).ToLowerInvariant() -eq ([string]$Configuration.manifest_sha256).ToLowerInvariant()
        }
    )
    if ($installed.Count -ne 1) {
        throw "The installed local agent model did not match the pinned release digest."
    }
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "SayTrace requires 64-bit Windows."
}
$osBuild = Get-SayTraceOsBuild
if ($osBuild -lt 22000) {
    throw "SayTrace requires Windows 11 or newer."
}

foreach ($requiredScript in @($InstallRuntimeScript, $VerifyWorkerScript, $DependencyManifest)) {
    if (-not (Test-Path -LiteralPath $requiredScript -PathType Leaf)) {
        throw "The SayTrace dependency preflight payload is incomplete."
    }
}
$dependencies = Get-Content -LiteralPath $DependencyManifest -Raw | ConvertFrom-Json
if (
    [int]$dependencies.schema_version -ne 1 -or
    [string]$dependencies.product -ne "SayTrace" -or
    [string]$dependencies.platform -ne "windows-x64"
) {
    throw "The dependency manifest does not match this SayTrace installer."
}
foreach ($url in @(
    [string]$dependencies.ollama.installer_url,
    [string]$dependencies.default_agent_model.manifest_url
)) {
    if (-not $url.StartsWith("https://", [StringComparison]::OrdinalIgnoreCase)) {
        throw "All dependency downloads must use HTTPS."
    }
}

$source = [IO.Path]::GetFullPath($SourceRuntime)
$destination = [IO.Path]::GetFullPath($DestinationRuntime)
$runtimeCheck = & $InstallRuntimeScript `
    -SourceRuntime $source `
    -DestinationRuntime $destination `
    -ValidateOnly `
    -SkipPayloadHashes

$destinationRoot = [IO.Path]::GetPathRoot($destination)
$drive = [IO.DriveInfo]::new($destinationRoot)
$requiredFreeSpace = [long]$runtimeCheck.payload_bytes + [long]$dependencies.additional_free_space_bytes
if ($drive.AvailableFreeSpace -lt $requiredFreeSpace) {
    $requiredGb = [Math]::Ceiling($requiredFreeSpace / 1GB)
    throw "SayTrace setup needs at least $requiredGb GB free on $destinationRoot for the app, local transcription, Ollama, and the starter agent model."
}

$workerCheck = & $VerifyWorkerScript `
    -WorkerExecutable (Join-Path $source "local-transcript-worker.exe") `
    -FfmpegExecutable (Join-Path $source "ffmpeg.exe") `
    -ExpectedProtocolVersion ([string]$runtimeCheck.worker_protocol_version) `
    -ExpectedPipelineVersion ([string]$runtimeCheck.pipeline_version)

$nvidiaAdapters = @(
    Get-CimInstance -ClassName Win32_VideoController -ErrorAction SilentlyContinue |
        Where-Object { ([string]$_.Name).Contains("NVIDIA") }
)
$gpuStatus = "CPU fallback ready"
if ($nvidiaAdapters.Count -gt 0) {
    $minimumDriver = [version][string]$dependencies.nvidia.minimum_ollama_driver_version
    $smiCandidates = @(
        (Join-Path $env:ProgramFiles "NVIDIA Corporation\NVSMI\nvidia-smi.exe")
    )
    $smiCommand = Get-Command nvidia-smi.exe -ErrorAction SilentlyContinue
    if ($smiCommand) {
        $smiCandidates += $smiCommand.Source
    }
    $smi = $smiCandidates |
        Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) } |
        Select-Object -First 1
    $driverText = ""
    if ($smi) {
        $driverText = (& $smi --query-gpu=driver_version --format=csv,noheader 2>$null |
            Select-Object -First 1).Trim()
    }
    $driverReady = $false
    if ($driverText) {
        try {
            $driverReady = [version]$driverText -ge $minimumDriver
        } catch {
            $driverReady = $false
        }
    }
    if (
        $driverReady -and
        [bool]$workerCheck.torch_cuda -and
        [bool]$workerCheck.ctranslate2_cuda
    ) {
        $gpuStatus = "NVIDIA GPU ready (driver $driverText)"
    } else {
        Write-Warning "An NVIDIA adapter was found, but the compatible GPU path did not pass every check. SayTrace will use its bundled CPU fallback; setup does not replace display drivers."
    }
}

$ollama = Get-SayTraceOllamaStatus
$minimumOllama = [version][string]$dependencies.ollama.minimum_version
$ollamaReady = $false
if ($ollama.available) {
    try {
        $ollamaReady = [version]$ollama.version -ge $minimumOllama
    } catch {
        $ollamaReady = $false
    }
}
if (-not $ollamaReady) {
    if ($ValidateOnly) {
        Write-Host "Ollama $($dependencies.ollama.installer_version) will be installed during setup."
    } else {
        Install-SayTraceOllama -Configuration $dependencies.ollama
        $ollamaExecutable = Resolve-SayTraceOllamaExecutable
        if (-not $ollamaExecutable) {
            throw "Ollama installed, but its local executable could not be found."
        }
        Start-SayTraceOllama -OllamaExecutable $ollamaExecutable
        $ollama = Wait-SayTraceOllama
        if (-not $ollama.available) {
            throw "Ollama installed, but its private loopback service did not become ready."
        }
    }
}

if ($ollama.available -and @($ollama.models).Count -eq 0) {
    if ($ValidateOnly) {
        Write-Host "The pinned $($dependencies.default_agent_model.name) starter model will be installed during setup."
    } else {
        $ollamaExecutable = Resolve-SayTraceOllamaExecutable
        if (-not $ollamaExecutable) {
            throw "Ollama is running, but its local executable could not be found."
        }
        Install-SayTraceDefaultModel `
            -OllamaExecutable $ollamaExecutable `
            -Configuration $dependencies.default_agent_model
        $ollama = Get-SayTraceOllamaStatus
    }
} elseif (-not $ollama.available -and -not $ValidateOnly) {
    throw "The local Ollama dependency is unavailable after installation."
}

$modelStatus = if ($ollama.available -and @($ollama.models).Count -gt 0) {
    "$(@($ollama.models).Count) local agent model(s) ready"
} elseif ($ValidateOnly) {
    "starter model scheduled for setup"
} else {
    "no local model ready"
}
Write-Host "SayTrace preflight passed: Windows build $osBuild; runtime $($runtimeCheck.runtime_version); $gpuStatus; $modelStatus."
