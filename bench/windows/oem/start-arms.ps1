# Start the two Windows arms for pair-check: the 1.18.31 freeze and ocserve.
#
# The fixture HOME must MATCH bench/parity/config exactly, otherwise the
# comparator reports configuration differences and calls them divergences. The
# first Windows run was missing `$schema`/`plugin`/`username` in the config and
# the whole models catalogue, which produced two "divergences" that were really
# an unseeded fixture.
$ErrorActionPreference = 'Stop'
$root = 'C:\ocserve'
$cfg  = Join-Path $root 'config'

foreach ($d in @("$root\freeze-home\.config\opencode",
                 "$root\freeze-home\.local\share\opencode",
                 "$root\freeze-home\.local\state\opencode",
                 "$root\freeze-home\.cache\opencode",
                 "$root\ocserve-home\.config\opencode",
                 "$root\ocserve-home\.local\share\opencode",
                 "$root\ocserve-home\.local\state\opencode",
                 "$root\ocserve-home\.cache\opencode",
                 "$root\ocserve-data")) {
  New-Item -ItemType Directory -Force -Path $d | Out-Null
}

# Same three files pair-check.sh copies on Linux, into the same XDG paths.
# NB: `$home` is a read-only automatic variable in PowerShell — using it as a
# loop variable aborts the script, which is how the first run silently ran with
# a stale fixture.
foreach ($h in @("$root\freeze-home", "$root\ocserve-home")) {
  Copy-Item "$cfg\opencode.json" "$h\.config\opencode\opencode.json" -Force
  Copy-Item "$cfg\auth.json"     "$h\.local\share\opencode\auth.json" -Force
  Copy-Item "$cfg\model.json"    "$h\.local\state\opencode\model.json" -Force
  if (Test-Path "$cfg\models.json") {
    Copy-Item "$cfg\models.json" "$h\.cache\opencode\models.json" -Force
  }
}

if (-not (Test-Path "$root\freeze.exe")) { throw "freeze.exe missing in $root" }
if (-not (Test-Path "$root\ocserve.exe")) { throw "ocserve.exe missing in $root" }

Get-Process ocserve,freeze -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2

# Freeze arm.
$env:HOME = "$root\freeze-home"
$env:USERPROFILE = "$root\freeze-home"
$env:OPENCODE_DISABLE_MODELS_FETCH = '1'
Start-Process -FilePath "$root\freeze.exe" -ArgumentList 'serve','--port','4926','--hostname','127.0.0.1' `
  -RedirectStandardOutput "$root\freeze.out.log" -RedirectStandardError "$root\freeze.err.log" `
  -WindowStyle Hidden
Start-Sleep -Seconds 8

# ocserve arm.
$env:OCSERVE_DATA_DIR = "$root\ocserve-data"
$env:OCSERVE_LEGACY_SYNC = '0'
$env:OCSERVE_SERVER_PASSWORD_FILE = ''
$env:RUST_LOG = 'warn'
Start-Process -FilePath "$root\ocserve.exe" -ArgumentList 'serve','--port','4927','--hostname','127.0.0.1' `
  -RedirectStandardOutput "$root\ocserve.out.log" -RedirectStandardError "$root\ocserve.err.log" `
  -WindowStyle Hidden
Start-Sleep -Seconds 8

foreach ($p in 4926, 4927) {
  $ok = Test-NetConnection -ComputerName 127.0.0.1 -Port $p -InformationLevel Quiet -WarningAction SilentlyContinue
  if (-not $ok) { throw "port $p never listened (see $root\*.err.log)" }
}
Write-Output 'arms up'
