# Installs the release .msi on a clean machine and proves it runs.
#
#   pwsh smoke/windows.ps1 -Msi <path-to.msi> -Version <expected-version>
#
# The app has no console in a release build, so nothing here reads stdout: the
# version comes from the installer's own records, and "does it start" is the
# exit code of `uia.exe --smoke`.
param(
  [Parameter(Mandatory)][string]$Msi,
  [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'

function Fail([string]$Message) { Write-Error "SMOKE FAIL: $Message"; exit 1 }

function Run-Msi([string[]]$MsiArgs, [string]$Log) {
  $p = Start-Process msiexec.exe -ArgumentList ($MsiArgs + @('/qn', '/norestart', '/l*v', "`"$Log`"")) -Wait -PassThru
  if ($p.ExitCode -ne 0) {
    if (Test-Path $Log) { Get-Content $Log -Tail 60 }
    Fail "msiexec $($MsiArgs -join ' ') exited $($p.ExitCode)"
  }
}

$Msi = (Resolve-Path $Msi).Path
Run-Msi @('/i', "`"$Msi`"") (Join-Path $env:RUNNER_TEMP 'msi-install.log')

# The uninstall registry is the installer's own record of what it installed.
$roots = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
         'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
$entry = Get-ItemProperty $roots -ErrorAction SilentlyContinue |
  Where-Object { $_.DisplayName -eq 'uia' } | Select-Object -First 1
if (-not $entry) {
  Get-ItemProperty $roots -ErrorAction SilentlyContinue | Select-Object DisplayName, DisplayVersion | Out-String | Write-Host
  Fail "no installed product named 'uia' in the uninstall registry"
}
if ($entry.DisplayVersion -ne $Version) {
  Fail "installed uia is $($entry.DisplayVersion), expected $Version"
}

$exe = if ($entry.InstallLocation) { Join-Path $entry.InstallLocation 'uia.exe' } else { Join-Path $env:ProgramFiles 'uia\uia.exe' }
if (-not (Test-Path $exe)) { Fail "uia.exe not found at $exe" }
Write-Host "installed uia $($entry.DisplayVersion) at $exe"

$run = Start-Process $exe -ArgumentList '--smoke' -Wait -PassThru
if ($run.ExitCode -ne 0) { Fail "'uia.exe --smoke' exited $($run.ExitCode)" }

Run-Msi @('/x', "`"$Msi`"") (Join-Path $env:RUNNER_TEMP 'msi-uninstall.log')
if (Test-Path $exe) { Fail "$exe is still there after uninstall" }
Write-Host "SMOKE OK: uia $Version"
