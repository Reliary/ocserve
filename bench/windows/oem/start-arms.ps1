# Start the two Windows arms for pair-check: the 1.18.31 freeze and ocserve.
# Both bind loopback inside the guest; scripts/windows-vm.sh tunnels 4926/4927.
$ErrorActionPreference = 'Stop'
$root = 'C:\ocserve'
New-Item -ItemType Directory -Force -Path "$root\freeze-home" | Out-Null
New-Item -ItemType Directory -Force -Path "$root\ocserve-data" | Out-Null

if (-not (Test-Path "$root\freeze.exe")) { throw "freeze.exe missing in $root" }
if (-not (Test-Path "$root\ocserve.exe")) { throw "ocserve.exe missing in $root" }

# Freeze: same fixture config the Linux pair uses, so the comparison is about
# code, not configuration.
$env:HOME = "$root\freeze-home"
$env:USERPROFILE = "$root\freeze-home"
$env:OPENCODE_DISABLE_MODELS_FETCH = '1'
Start-Process -FilePath "$root\freeze.exe" -ArgumentList 'serve','--port','4926' `
  -RedirectStandardOutput "$root\freeze.out.log" -RedirectStandardError "$root\freeze.err.log" `
  -WindowStyle Hidden
Start-Sleep -Seconds 6

$env:OCSERVE_DATA_DIR = "$root\ocserve-data"
$env:OCSERVE_LEGACY_SYNC = '0'
$env:OCSERVE_SERVER_PASSWORD_FILE = ''
$env:RUST_LOG = 'warn'
Start-Process -FilePath "$root\ocserve.exe" -ArgumentList 'serve','--port','4927' `
  -RedirectStandardOutput "$root\ocserve.out.log" -RedirectStandardError "$root\ocserve.err.log" `
  -WindowStyle Hidden
Start-Sleep -Seconds 6

foreach ($p in 4926, 4927) {
  $ok = Test-NetConnection -ComputerName 127.0.0.1 -Port $p -InformationLevel Quiet -WarningAction SilentlyContinue
  if (-not $ok) { throw "port $p never listened (see $root\*.err.log)" }
}
Write-Output 'arms up'
