#requires -Version 7.2

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$WorkerExecutable,

    [Parameter(Mandatory)]
    [string]$FfmpegExecutable,

    [Parameter()]
    [switch]$RequireNvidia
)

$ErrorActionPreference = "Stop"
$worker = [IO.Path]::GetFullPath($WorkerExecutable)
$ffmpeg = [IO.Path]::GetFullPath($FfmpegExecutable)
foreach ($file in @($worker, $ffmpeg)) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        throw "Required runtime executable does not exist: $file"
    }
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

$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) "LocalTranscriptWorkerSmoke-$([Guid]::NewGuid().ToString('N'))"
$modelRoot = Join-Path $temporaryRoot "models"
$allowedRoot = Join-Path $temporaryRoot "library"
[IO.Directory]::CreateDirectory($modelRoot) | Out-Null
[IO.Directory]::CreateDirectory($allowedRoot) | Out-Null

$process = $null
$stderrTask = $null
try {
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $worker
    foreach ($argument in @(
        "--model-root",
        $modelRoot,
        "--allowed-root",
        $allowedRoot,
        "--ffmpeg",
        $ffmpeg,
        "--heartbeat-seconds",
        "3600"
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
        throw "The packaged worker handshake is incompatible with this application."
    }

    $healthId = "runtime-health-$([Guid]::NewGuid().ToString('N'))"
    Write-WorkerMessage -Stream $input -Value ([ordered]@{
        protocol_version = "1.0"
        type = "request"
        request_id = $healthId
        command = "health"
        payload = @{}
    })
    do {
        $message = Read-WorkerMessage -Stream $output
    } while ([string]$message.request_id -ne $healthId)
    if (-not [bool]$message.ok -or [string]$message.result.status -ne "ok") {
        throw "The packaged worker health check failed."
    }
    if (
        $RequireNvidia -and
        (
            -not [bool]$message.result.gpu.torch_cuda -or
            -not [bool]$message.result.gpu.ctranslate2_cuda
        )
    ) {
        throw "The packaged NVIDIA worker did not expose both PyTorch and CTranslate2 CUDA backends."
    }

    $shutdownId = "runtime-shutdown-$([Guid]::NewGuid().ToString('N'))"
    Write-WorkerMessage -Stream $input -Value ([ordered]@{
        protocol_version = "1.0"
        type = "request"
        request_id = $shutdownId
        command = "shutdown"
        payload = @{}
    })
    $process.StandardInput.Close()
    if (-not $process.WaitForExit(15000)) {
        throw "The packaged worker did not shut down after its smoke test."
    }
    if ($process.ExitCode -ne 0) {
        $stderr = if ($stderrTask) { $stderrTask.GetAwaiter().GetResult() } else { "" }
        throw "The packaged worker exited with code $($process.ExitCode): $($stderr.Trim())"
    }

    [pscustomobject]@{
        protocol_version = [string]$hello.protocol_version
        pipeline_version = [string]$hello.payload.pipeline_version
        worker_version = [string]$message.result.worker_version
        torch_cuda = [bool]$message.result.gpu.torch_cuda
        ctranslate2_cuda = [bool]$message.result.gpu.ctranslate2_cuda
        ctranslate2_device_count = [int]$message.result.gpu.ctranslate2_device_count
        network_mode = [string]$message.result.network_mode
    }
} finally {
    if ($process -and -not $process.HasExited) {
        $process.Kill($true)
        $process.WaitForExit()
    }
    $resolvedTemporary = [IO.Path]::GetFullPath($temporaryRoot)
    $systemTemporary = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (
        $resolvedTemporary.StartsWith($systemTemporary, [StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path -Leaf $resolvedTemporary).StartsWith(
            "LocalTranscriptWorkerSmoke-",
            [StringComparison]::Ordinal
        )
    ) {
        Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force -ErrorAction SilentlyContinue
    }
}
