[CmdletBinding()]
param(
    [string] $OutputDirectory = "windows/dist/selection-translate-x64"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$windowsRoot = Join-Path $repoRoot "windows"
$cargo = "D:\DevTools\cargo\bin\cargo.exe"
$vsDevCmd = "D:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat"
$rustupHome = "D:\DevTools\rustup"
$cargoHome = "D:\DevTools\cargo"
$buildTemp = Join-Path $windowsRoot "tmp"
$targetDirectory = Join-Path $windowsRoot "target"
$releaseDirectory = Join-Path $targetDirectory "release"
$configTemplate = Join-Path $repoRoot "windows\config\config.example.toml"
$lockFile = Join-Path $repoRoot "Cargo.lock"

foreach ($requiredPath in @($cargo, $vsDevCmd, $configTemplate, $lockFile)) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required tool or source file was not found: $requiredPath"
    }
}

if ([IO.Path]::IsPathRooted($OutputDirectory)) {
    $outputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
} else {
    $outputDirectory = [IO.Path]::GetFullPath((Join-Path $repoRoot $OutputDirectory))
}
$distRoot = [IO.Path]::GetFullPath((Join-Path $windowsRoot "dist"))
$distPrefix = $distRoot.TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
if (-not $outputDirectory.StartsWith($distPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputDirectory must stay inside '$distRoot'."
}

New-Item -ItemType Directory -Path $buildTemp -Force | Out-Null
if (Test-Path -LiteralPath $outputDirectory) {
    if (-not (Get-Item -LiteralPath $outputDirectory -ErrorAction Stop).PSIsContainer) {
        throw "OutputDirectory exists as a file: $outputDirectory"
    }
    $existingOutput = @(Get-ChildItem -LiteralPath $outputDirectory -Force)
    if ($existingOutput.Count -gt 0) {
        throw "OutputDirectory is not empty; refusing to mix stale files into a package: $outputDirectory"
    }
} else {
    New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null
}

# Keep Cargo's downloads, target files, and compiler temporary files on D:.
$env:RUSTUP_HOME = $rustupHome
$env:CARGO_HOME = $cargoHome
$env:CARGO_TARGET_DIR = $targetDirectory
$env:TEMP = $buildTemp
$env:TMP = $buildTemp

function Invoke-VsCargo {
    param([Parameter(Mandatory = $true)][string[]] $CargoArguments)

    $quotedArguments = $CargoArguments | ForEach-Object {
        '"' + ($_ -replace '"', '\"') + '"'
    }
    $commandLine = 'call "{0}" -arch=x64 && "{1}" {2}' -f $vsDevCmd, $cargo, ($quotedArguments -join " ")
    Push-Location $repoRoot
    try {
        & $env:ComSpec /d /s /c $commandLine
        if ($LASTEXITCODE -ne 0) {
            throw "Cargo command failed with exit code $($LASTEXITCODE): cargo $($CargoArguments -join ' ')"
        }
    } finally {
        Pop-Location
    }
}

# Every package run verifies the source and produces the binaries that are copied.
# Keeping these checks in the script prevents a stale target directory from being
# mistaken for a verified package. The output directory is still untouched until
# all commands below have succeeded.
Invoke-VsCargo -CargoArguments @("fmt", "--all", "--", "--check")
Invoke-VsCargo -CargoArguments @("test", "--workspace", "--locked")
Invoke-VsCargo -CargoArguments @("clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings")
Invoke-VsCargo -CargoArguments @(
    "build", "--locked", "--release",
    "-p", "selection-translate-resident",
    "-p", "selection-translate-manager"
)

$artifacts = @(
    @{
        Source = Join-Path $releaseDirectory "selection-translate-resident.exe"
        Name = "selection-translate-resident.exe"
    },
    @{
        Source = Join-Path $releaseDirectory "selection-translate-manager.exe"
        Name = "selection-translate-manager.exe"
    }
)
foreach ($artifact in $artifacts) {
    if (-not (Test-Path -LiteralPath $artifact.Source -PathType Leaf)) {
        throw "Release artifact was not produced: $($artifact.Source)"
    }
    Copy-Item -LiteralPath $artifact.Source -Destination (Join-Path $outputDirectory $artifact.Name) -Force
}

Copy-Item -LiteralPath $configTemplate -Destination (Join-Path $outputDirectory "config.example.toml") -Force
$documentation = @(
    "README.md",
    "docs\SETUP.md",
    "docs\HOTKEYS.md",
    "docs\FALLBACKS.md",
    "docs\PRIVACY.md",
    "docs\TROUBLESHOOTING.md",
    "docs\VERIFICATION.md"
)
$packageDocs = Join-Path $outputDirectory "docs"
New-Item -ItemType Directory -Path $packageDocs -Force | Out-Null
foreach ($document in $documentation) {
    $source = Join-Path $repoRoot $document
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Documentation source was not found: $source"
    }
    $destination = if ($document -eq "README.md") {
        Join-Path $outputDirectory "README.md"
    } else {
        Join-Path $packageDocs ([IO.Path]::GetFileName($document))
    }
    Copy-Item -LiteralPath $source -Destination $destination -Force
}

$gitCommand = Get-Command git.exe -ErrorAction SilentlyContinue
if ($null -ne $gitCommand) {
    $gitErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $gitCommit = (& $gitCommand.Source -C $repoRoot rev-parse --verify HEAD 2>$null | Select-Object -First 1)
    } finally {
        $ErrorActionPreference = $gitErrorAction
    }
    $gitExitCode = $LASTEXITCODE
    if ($gitExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($gitCommit)) {
        $gitCommit = "uncommitted (no commit available)"
    }
} else {
    $gitCommit = "unavailable"
}

$buildInfo = @(
    "Selection Translate local portable package",
    "Architecture: x86_64-pc-windows-msvc",
    "Built UTC: $([DateTimeOffset]::UtcNow.ToString('o'))",
    "Rust/Cargo: $cargo",
    "Visual Studio environment: $vsDevCmd -arch=x64",
    "Cargo.lock SHA-256: $((Get-FileHash -LiteralPath $lockFile -Algorithm SHA256).Hash)",
    "Git commit: $gitCommit",
    "Artifacts: selection-translate-resident.exe, selection-translate-manager.exe",
    "This folder is unsigned and is for local testing only."
)
$buildInfo | Set-Content -LiteralPath (Join-Path $outputDirectory "BUILD-INFO.txt") -Encoding UTF8

Write-Output "Portable package created: $outputDirectory"
Get-ChildItem -LiteralPath $outputDirectory -Recurse -File |
    Sort-Object FullName |
    Select-Object FullName, Length
# A missing optional Git commit is represented in BUILD-INFO and must not
# leak git.exe's exit code into an otherwise successful package invocation.
$global:LASTEXITCODE = 0
