[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string] $ProcessName = "",
    [ValidateRange(1, [int]::MaxValue)]
    [int] $ProcessId = 0,
    [ValidateRange(1, 86400)]
    [int] $DurationSeconds = 300,
    [ValidateRange(1, 60)]
    [int] $SampleIntervalSeconds = 1,
    [string] $OutputPath = "",
    [ValidateRange(0, 1048576)]
    [double] $MaxPrivateWorkingSetMiB = 0,
    [ValidateRange(0, 100)]
    [double] $MaxAverageCpuPercent = 0
)

if ([string]::IsNullOrWhiteSpace($ProcessName) -and $ProcessId -eq 0) {
    throw "Specify exactly one target with -ProcessName or -ProcessId."
}
if (-not [string]::IsNullOrWhiteSpace($ProcessName) -and $ProcessId -ne 0) {
    throw "Specify exactly one target with -ProcessName or -ProcessId, not both."
}

$measureDirectory = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\tmp"))
if (-not (Test-Path -LiteralPath $measureDirectory -PathType Container)) {
    New-Item -ItemType Directory -Path $measureDirectory -Force | Out-Null
}
$defaultOutputLabel = if ($ProcessId -ne 0) { "pid-$ProcessId" } else { $ProcessName }
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $OutputPath = Join-Path $measureDirectory "memory-$defaultOutputLabel.csv"
} else {
    $OutputPath = [IO.Path]::GetFullPath((Join-Path (Get-Location) $OutputPath))
}
$measurePrefix = $measureDirectory.TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
if (-not $OutputPath.StartsWith($measurePrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputPath must be inside '$measureDirectory'."
}

function Resolve-PrivateWorkingSetCounterPath {
    param([Diagnostics.Process] $TargetProcess)

    for ($attempt = 0; $attempt -lt 10; $attempt++) {
        try {
            $samples = (Get-Counter -Counter '\Process(*)\ID Process' -ErrorAction Stop).CounterSamples
            $idSample = $samples |
                Where-Object { $_.Status -eq 0 -and $_.Path -match '\\id process$' -and [int64]$_.CookedValue -eq $TargetProcess.Id } |
                Select-Object -First 1
            if ($null -ne $idSample) {
                return "\Process($($idSample.InstanceName))\Working Set - Private"
            }
        } catch {
            if ($attempt -eq 9) {
                throw
            }
        }
        Start-Sleep -Milliseconds 250
    }
    throw "Could not resolve the process instance for PID $($TargetProcess.Id)."
}

function Get-PrivateWorkingSetMiB {
    param([string] $CounterPath)

    for ($attempt = 0; $attempt -lt 5; $attempt++) {
        try {
            $sample = (Get-Counter -Counter $CounterPath -SampleInterval $SampleIntervalSeconds -MaxSamples 1 -ErrorAction Stop).CounterSamples |
                Where-Object { $_.Status -eq 0 } |
                Select-Object -First 1
            if ($null -ne $sample) {
                return [double]$sample.CookedValue / 1MB
            }
        } catch {
            if ($attempt -eq 4) {
                throw
            }
        }
        Start-Sleep -Milliseconds 100
    }
    throw "Private working-set counter returned no sample."
}

$process = if ($ProcessId -ne 0) {
    Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
} else {
    $matchingProcesses = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue)
    if ($matchingProcesses.Count -gt 1) {
        throw "Multiple processes named '$ProcessName' were found. Use -ProcessId to select the resident explicitly."
    }
    $matchingProcesses | Select-Object -First 1
}
if ($null -eq $process) {
    $targetDescription = if ($ProcessId -ne 0) { "PID $ProcessId" } else { "process '$ProcessName'" }
    throw "$targetDescription was not found. Start the resident before measuring it."
}

$processorCount = [Environment]::ProcessorCount
$samples = [System.Collections.Generic.List[object]]::new()
$privateCounterPath = Resolve-PrivateWorkingSetCounterPath -TargetProcess $process
$previousCpu = $process.TotalProcessorTime
$previousTime = [Diagnostics.Stopwatch]::GetTimestamp()
$deadline = [DateTime]::UtcNow.AddSeconds($DurationSeconds)

while ([DateTime]::UtcNow -lt $deadline) {
    try {
        $process.Refresh()
        $now = [Diagnostics.Stopwatch]::GetTimestamp()
        $cpu = $process.TotalProcessorTime
        $elapsedSeconds = ($now - $previousTime) / [Diagnostics.Stopwatch]::Frequency
        $cpuSeconds = ($cpu - $previousCpu).TotalSeconds
        $cpuPercent = if ($elapsedSeconds -gt 0) { 100.0 * $cpuSeconds / $elapsedSeconds / $processorCount } else { 0.0 }
        $privateWorkingSetMiB = Get-PrivateWorkingSetMiB -CounterPath $privateCounterPath
        $samples.Add([pscustomobject]@{
            TimestampUtc = [DateTimeOffset]::UtcNow.ToString('o')
            ProcessId = $process.Id
            PrivateWorkingSetMiB = [Math]::Round($privateWorkingSetMiB, 3)
            CpuPercent = [Math]::Round($cpuPercent, 4)
        })
        $previousCpu = $cpu
        $previousTime = $now
    } catch [System.ArgumentException] {
        break
    } catch [System.InvalidOperationException] {
        break
    } catch [System.ComponentModel.Win32Exception] {
        break
    }
}

if ($samples.Count -eq 0) {
    throw "No samples were collected; the process may have exited."
}

$samples | Export-Csv -LiteralPath $OutputPath -NoTypeInformation -Encoding UTF8
$avgCpu = ($samples | Measure-Object -Property CpuPercent -Average).Average
$avgMemory = ($samples | Measure-Object -Property PrivateWorkingSetMiB -Average).Average
$maxMemory = ($samples | Measure-Object -Property PrivateWorkingSetMiB -Maximum).Maximum
Write-Output ("Samples: {0}; average CPU: {1:N4}%; average private working set: {2:N3} MiB; peak: {3:N3} MiB; report: {4}" -f $samples.Count, $avgCpu, $avgMemory, $maxMemory, (Resolve-Path -LiteralPath $OutputPath))

if ($MaxPrivateWorkingSetMiB -gt 0 -and $maxMemory -gt $MaxPrivateWorkingSetMiB) {
    throw "Private working-set budget failed: peak $([Math]::Round($maxMemory, 3)) MiB exceeds $MaxPrivateWorkingSetMiB MiB."
}
if ($MaxAverageCpuPercent -gt 0 -and $avgCpu -gt $MaxAverageCpuPercent) {
    throw "Average CPU budget failed: $([Math]::Round($avgCpu, 4))% exceeds $MaxAverageCpuPercent%."
}
