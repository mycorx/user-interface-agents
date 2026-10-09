#!/usr/bin/env pwsh
# Builds a local Windows installer (MSI) for the uia desktop app.
# Run from anywhere; produces crates/uia-app/target/release/bundle/msi/*.msi

# NOTE: $ErrorActionPreference governs PowerShell cmdlets ONLY -- it does
# nothing for native commands like pnpm. Their failure sets $LASTEXITCODE and
# otherwise lets the script run on, so every native call below is followed by
# an explicit check. Without them this script printed "Done" and listed a
# stale MSI from a previous run after a build that had actually failed, and
# exited 0 while doing it.
$ErrorActionPreference = "Stop"

function Invoke-Checked {
    param(
        [Parameter(Mandatory)][scriptblock] $Command,
        [Parameter(Mandatory)][string] $What
    )
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$What failed (exit code $LASTEXITCODE)"
    }
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $repoRoot

# The linker cannot overwrite uia.exe while a copy of it is running, and the
# failure it produces ("Access is denied. (os error 5)") names neither the app
# nor the reason. Checked up front so the fix is obvious instead of archaeology.
$running = Get-Process uia -ErrorAction SilentlyContinue
if ($running) {
    throw ("uia is running (PID $($running.Id -join ', ')), and the build cannot replace " +
           "its executable while it is. Close the app and run this again.")
}

Write-Host "==> Installing frontend dependencies" -ForegroundColor Cyan
Invoke-Checked { pnpm install } "pnpm install"

Write-Host "==> Building Tauri bundle (msi)" -ForegroundColor Cyan
# Recorded before the build so the freshness check below can tell this run's
# output from a leftover. One second back, because the MSI's timestamp and
# this clock reading can land in the same tick.
$startedAt = (Get-Date).AddSeconds(-1)
Push-Location crates/uia-app
try {
    # createUpdaterArtifacts (tauri.conf.json) makes the bundler sign an updater
    # bundle, and it fails without the release key. A local build is not an
    # update, so turn the artifacts off unless the key is in the environment.
    # The override goes in a file because `--config` takes a path as well as a
    # JSON string, and a path survives PowerShell's native-argument quoting
    # (which differs between 5.1 and 7.3+, and for pnpm.cmd vs pnpm.exe).
    if ($env:TAURI_SIGNING_PRIVATE_KEY) {
        Invoke-Checked { pnpm exec tauri build } "tauri build"
    } else {
        $noUpdaterConfig = Join-Path ([System.IO.Path]::GetTempPath()) "uia-no-updater-artifacts.json"
        Set-Content -Path $noUpdaterConfig -Value '{"bundle":{"createUpdaterArtifacts":false}}' -Encoding ascii
        Invoke-Checked { pnpm exec tauri build --config $noUpdaterConfig } "tauri build"
    }
}
finally {
    Pop-Location
}

$bundleDir = Join-Path $repoRoot "target/release/bundle/msi"
if (-not (Test-Path $bundleDir)) {
    $bundleDir = Join-Path $repoRoot "crates/uia-app/target/release/bundle/msi"
}

# Anything on disk here could be left over from an earlier run, so "an MSI
# exists" is not evidence this run produced one. Only files written since the
# build started count.
$installers = @(
    Get-ChildItem -Path $bundleDir -Filter "*.msi" -ErrorAction SilentlyContinue |
        Where-Object { $_.LastWriteTime -ge $startedAt }
)
if ($installers.Count -eq 0) {
    throw "the build reported success but produced no MSI in $bundleDir"
}

Write-Host "==> Done. Installer(s):" -ForegroundColor Green
$installers | ForEach-Object { Write-Host "  $($_.FullName)" }
