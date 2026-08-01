#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$WorkerExecutable,

    [Parameter(Mandatory)]
    [string]$FfmpegExecutable,

    [Parameter(Mandatory)]
    [string]$ModelRoot,

    [Parameter(Mandatory)]
    [string]$ProbeRoot,

    [Parameter(Mandatory)]
    [string]$SourcePath
)

$ErrorActionPreference = "Stop"
$worker = [IO.Path]::GetFullPath($WorkerExecutable)
$ffmpeg = [IO.Path]::GetFullPath($FfmpegExecutable)
$models = [IO.Path]::GetFullPath($ModelRoot)
$allowedRoot = [IO.Path]::GetFullPath($ProbeRoot)
$source = [IO.Path]::GetFullPath($SourcePath)
$workspace = Join-Path $allowedRoot "work"

foreach ($file in @($worker, $ffmpeg, $source)) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        throw "Required probe file does not exist: $file"
    }
}
foreach ($directory in @($models, $allowedRoot, $workspace)) {
    if (-not (Test-Path -LiteralPath $directory -PathType Container)) {
        throw "Required probe directory does not exist: $directory"
    }
}
if (
    -not $source.StartsWith("$allowedRoot\", [StringComparison]::OrdinalIgnoreCase) -or
    -not $workspace.StartsWith("$allowedRoot\", [StringComparison]::OrdinalIgnoreCase)
) {
    throw "Probe inputs must remain inside ProbeRoot."
}

function Read-WorkerBytes {
    param(
        [Parameter(Mandatory)]
        [IO.Stream]$Stream,

        [Parameter(Mandatory)]
        [int]$Count
    )

    $buffer = [byte[]]::new($Count)
    $offset = 0
    while ($offset -lt $Count) {
        $read = $Stream.Read($buffer, $offset, $Count - $offset)
        if ($read -le 0) {
            throw "The packaged worker closed its output pipe unexpectedly."
        }
        $offset += $read
    }
    return ,$buffer
}

function Read-WorkerMessage {
    param([Parameter(Mandatory)][IO.Stream]$Stream)

    $header = Read-WorkerBytes -Stream $Stream -Count 16
    if (
        [Text.Encoding]::ASCII.GetString($header, 0, 4) -ne "LTW1" -or
        $header[4] -ne 1 -or
        $header[5] -ne 1
    ) {
        throw "The packaged worker returned an incompatible frame header."
    }
    $lengthBytes = [byte[]]$header[8..15]
    if ([BitConverter]::IsLittleEndian) {
        [Array]::Reverse($lengthBytes)
    }
    $length = [BitConverter]::ToUInt64($lengthBytes, 0)
    if ($length -gt 4MB) {
        throw "The packaged worker returned an oversized control frame."
    }
    $payload = Read-WorkerBytes -Stream $Stream -Count ([int]$length)
    return [Text.Encoding]::UTF8.GetString($payload) | ConvertFrom-Json
}

function Write-WorkerMessage {
    param(
        [Parameter(Mandatory)]
        [IO.Stream]$Stream,

        [Parameter(Mandatory)]
        [object]$Value
    )

    $payload = [Text.Encoding]::UTF8.GetBytes(($Value | ConvertTo-Json -Compress -Depth 12))
    $lengthBytes = [BitConverter]::GetBytes([uint64]$payload.Length)
    if ([BitConverter]::IsLittleEndian) {
        [Array]::Reverse($lengthBytes)
    }
    $header = [byte[]]::new(16)
    [Text.Encoding]::ASCII.GetBytes("LTW1").CopyTo($header, 0)
    $header[4] = 1
    $header[5] = 1
    $lengthBytes.CopyTo($header, 8)
    $Stream.Write($header, 0, $header.Length)
    $Stream.Write($payload, 0, $payload.Length)
    $Stream.Flush()
}

$process = $null
$stderrTask = $null
$pipelineResult = $null
$pipelineError = $null
$stageEvents = [Collections.Generic.List[object]]::new()
$jobId = "packaged-quality-probe"
$requestId = "probe-$([Guid]::NewGuid().ToString('N'))"

try {
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $worker
    foreach ($argument in @(
        "--model-root",
        $models,
        "--allowed-root",
        $allowedRoot,
        "--ffmpeg",
        $ffmpeg,
        "--heartbeat-seconds",
        "30"
    )) {
        $start.ArgumentList.Add($argument)
    }
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    if (-not $process.Start()) {
        throw "The packaged worker process could not be started."
    }
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $output = $process.StandardOutput.BaseStream
    $input = $process.StandardInput.BaseStream

    $hello = Read-WorkerMessage -Stream $output
    if (
        [string]$hello.event -ne "hello" -or
        [string]$hello.protocol_version -ne "1.0" -or
        [string]$hello.payload.pipeline_version -ne "2026.07.28.1"
    ) {
        throw "The packaged worker handshake is incompatible with this probe."
    }

    Write-WorkerMessage -Stream $input -Value ([ordered]@{
        protocol_version = "1.0"
        type = "request"
        request_id = $requestId
        command = "pipeline.run"
        job_id = $jobId
        pipeline_version = "2026.07.28.1"
        payload = [ordered]@{
            workspace_path = $workspace
            sources = @([ordered]@{
                asset_id = "probe-audio"
                path = $source
                source_type = "import"
                priority = 20
            })
            diarization_asset_id = "probe-audio"
            profiles = @()
            resume = @{}
        }
    })

    while ($null -eq $pipelineResult -and $null -eq $pipelineError) {
        $message = Read-WorkerMessage -Stream $output
        if ([string]$message.event -eq "job_progress") {
            $stageEvents.Add($message.payload)
        } elseif (
            [string]$message.event -eq "job_complete" -and
            [string]$message.payload.job_id -eq $jobId
        ) {
            $pipelineResult = $message.payload.result
        } elseif (
            [string]$message.event -eq "job_error" -and
            [string]$message.payload.job_id -eq $jobId
        ) {
            $pipelineError = $message.payload.error
        }
    }

    if ($null -ne $pipelineError) {
        throw "Packaged pipeline failed: $($pipelineError | ConvertTo-Json -Compress -Depth 12)"
    }

    $shutdownId = "probe-shutdown-$([Guid]::NewGuid().ToString('N'))"
    Write-WorkerMessage -Stream $input -Value ([ordered]@{
        protocol_version = "1.0"
        type = "request"
        request_id = $shutdownId
        command = "shutdown"
        payload = @{}
    })
    do {
        $message = Read-WorkerMessage -Stream $output
    } while ([string]$message.request_id -ne $shutdownId)
    $process.StandardInput.Close()
    if (-not $process.WaitForExit(30000)) {
        throw "The packaged worker did not shut down after the pipeline probe."
    }
    if ($process.ExitCode -ne 0) {
        $stderr = if ($stderrTask) { $stderrTask.GetAwaiter().GetResult() } else { "" }
        throw "The packaged worker exited with code $($process.ExitCode): $($stderr.Trim())"
    }

    [pscustomobject]@{
        protocol_version = [string]$hello.protocol_version
        pipeline_version = [string]$hello.payload.pipeline_version
        network_mode = [string]$hello.payload.network_mode
        turn_count = [int]$pipelineResult.turn_count
        word_count = [int]$pipelineResult.word_count
        speaker_count = @($pipelineResult.speaker_candidates).Count
        warning_count = @($pipelineResult.warnings).Count
        warnings = @($pipelineResult.warnings)
        canonical_artifact_path = [string]$pipelineResult.canonical_artifact_path
        progress_event_count = $stageEvents.Count
    }
} finally {
    if ($process -and -not $process.HasExited) {
        $process.Kill($true)
        $process.WaitForExit()
    }
}
