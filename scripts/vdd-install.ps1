# Install the packaged display driver (target/vdd-package, scripts/package-vdd.sh)
# and its device, or remove both with -Remove. Run from an elevated shell. The
# device is made once; installing again updates the driver of the one there.

param([switch]$Remove)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$kit = (& python.exe (Join-Path $root 'scripts\fetch-wdk.py')).Trim()
$devcon = Join-Path $kit 'c\tools\10.0.26100.0\x64\devcon.exe'
$hwid = 'Root\lowlat-vdd'
$inf = Join-Path $root 'target\vdd-package\lowlat-vdd.inf'

function Present {
    $found = & $devcon find $hwid
    return -not ($found -match 'No matching devices found')
}

if ($Remove) {
    if (Present) { & $devcon remove $hwid }
    # Every package of this driver in the store, by the name it was built as.
    $text = (pnputil /enum-drivers) -join "`n"
    $blocks = $text -split "`n\s*`n"
    foreach ($block in $blocks) {
        if ($block -match 'Original Name:\s+lowlat-vdd\.inf' -and $block -match 'Published Name:\s+(oem\d+\.inf)') {
            pnputil /delete-driver $Matches[1] /uninstall /force
        }
    }
    return
}

if (-not (Test-Path $inf)) { throw "no package at $inf; run scripts/package-vdd.sh first" }
if (Present) {
    & $devcon update $inf $hwid
} else {
    & $devcon install $inf $hwid
}
