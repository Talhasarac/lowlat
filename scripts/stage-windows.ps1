#!/usr/bin/env pwsh
# Build the folder W2.5's install copies from: the host, the display
# controller, the display driver's package when one has been built, the login
# tool and the configuration template.
#
#   scripts/stage-windows.ps1 [-Profile release] [-OutDir <path>]
#
# The package takes the driver from target/vdd-package, which
# scripts/package-vdd.sh signs; a folder without one is still staged, with a
# warning, for a machine that already has the driver. The result is what the
# Windows zip will carry (docs/impl-plan-windows.md W2.5).

[CmdletBinding()]
param(
    [string]$Profile = "release",
    [string]$OutDir = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) {
    $OutDir = Join-Path $root "target\staging"
}
$built = Join-Path $root "target\$Profile"

function Take([string]$Source, [string]$Name) {
    if (-not (Test-Path -LiteralPath $Source)) {
        throw "missing $Source; build it first"
    }
    Copy-Item -LiteralPath $Source -Destination (Join-Path $OutDir $Name) -Force
    Write-Host "  $Name"
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
Write-Host "staging in $OutDir"

Take (Join-Path $built "lowlatd.exe") "lowlatd.exe"
Take (Join-Path $built "lowlat-vdd.exe") "lowlat-vdd.exe"
Take (Join-Path $root "scripts\kessel-login.py") "lowlat-login.py"
Take (Join-Path $root "packaging\lowlatd.env.windows") "lowlatd.env.template"

$parts = @("lowlat_idd.dll", "lowlat-idd.inf", "lowlat-idd.cat")
$package = Join-Path $root "target\vdd-package"
$missing = $parts | Where-Object { -not (Test-Path -LiteralPath (Join-Path $package $_)) }
if ($missing) {
    Write-Warning "no complete driver package in $package (scripts/package-vdd.sh builds it); the folder has no driver"
} else {
    $driver = Join-Path $OutDir "driver"
    New-Item -ItemType Directory -Path $driver -Force | Out-Null
    foreach ($part in $parts) {
        Copy-Item -LiteralPath (Join-Path $package $part) -Destination (Join-Path $driver $part) -Force
        Write-Host "  driver\$part"
    }
}
