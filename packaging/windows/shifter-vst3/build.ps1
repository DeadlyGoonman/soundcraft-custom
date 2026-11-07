# Builds the Shifter VST3 plugin (crate soundcraft-shifter-vst3) and assembles the bundle:
#
#   <OutDir>\Shifter.vst3\Contents\x86_64-win\Shifter.vst3   (the cdylib, renamed)
#   <OutDir>\Shifter.vst3\Contents\Resources\                (empty, reserved)
#
# Does not install anything. To use it, copy Shifter.vst3 into a VST3 folder your host scans
# (C:\Program Files\Common Files\VST3 for most hosts) or add <OutDir> to the host's plugin paths.
#
# Usage: powershell -ExecutionPolicy Bypass -File packaging\windows\shifter-vst3\build.ps1 [-Debug]
#        [-TargetDir D:\ai\Projects\bt-c15-debug] [-OutDir D:\ai\Projects\shifter-vst3-out]

param(
    [string]$TargetDir = "D:\ai\Projects\bt-c15-debug",
    [string]$OutDir = "D:\ai\Projects\shifter-vst3-out",
    [switch]$Debug
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..\..\..")
$env:CARGO_TARGET_DIR = $TargetDir

$cargoArgs = @("build", "-p", "soundcraft-shifter-vst3", "--lib")
$profileDir = "debug"
if (-not $Debug) {
    $cargoArgs += "--release"
    $profileDir = "release"
}

Push-Location $repo
try {
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}

$dll = Join-Path $TargetDir "$profileDir\Shifter.dll"
if (-not (Test-Path $dll)) { throw "not built: $dll" }

$bundle = Join-Path $OutDir "Shifter.vst3"
if (Test-Path $bundle) { Remove-Item -Recurse -Force $bundle }
$bin = Join-Path $bundle "Contents\x86_64-win"
New-Item -ItemType Directory -Force $bin | Out-Null
New-Item -ItemType Directory -Force (Join-Path $bundle "Contents\Resources") | Out-Null
Copy-Item $dll (Join-Path $bin "Shifter.vst3")

Write-Host "Bundle: $bundle"
Get-ChildItem -Recurse $bundle | Select-Object FullName, Length | Format-Table -AutoSize
