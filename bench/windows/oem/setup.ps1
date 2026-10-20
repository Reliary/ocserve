# ocserve Windows VM provisioning (runs once, at the end of the unattended install).
#
# Deliberately small: enable SSH so the Linux side can drive the guest, then
# stop. Everything ocserve-specific is pushed later by scripts/windows-vm.sh, so
# changing the test payload never requires re-running this.
$ErrorActionPreference = 'Continue'
$log = 'C:\OEM\setup.log'
function Say($m) { $t = (Get-Date).ToString('s'); "$t $m" | Tee-Object -FilePath $log -Append }

Say 'setup start'

# --- OpenSSH Server ---------------------------------------------------------
$cap = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' -ErrorAction SilentlyContinue
if ($cap -and $cap.State -ne 'Installed') {
    Say 'installing OpenSSH.Server capability'
    try { Add-WindowsCapability -Online -Name $cap.Name -ErrorAction Stop | Out-Null }
    catch { Say "capability install failed: $_" }
}
if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
    # Fallback: Win32-OpenSSH MSI from GitHub. LTSC and Server images sometimes
    # lack the Features-on-Demand payload.
    Say 'capability unavailable; installing Win32-OpenSSH from GitHub'
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    for ($i = 0; $i -lt 10; $i++) {
        try {
            $rel = Invoke-RestMethod -Uri 'https://api.github.com/repos/PowerShell/Win32-OpenSSH/releases/latest' -UseBasicParsing
            $msi = $rel.assets | Where-Object { $_.name -match '^OpenSSH-Win64-.*\.msi$' } | Select-Object -First 1
            if ($msi) {
                $out = 'C:\OEM\openssh.msi'
                Invoke-WebRequest -Uri $msi.browser_download_url -OutFile $out -UseBasicParsing
                Start-Process msiexec.exe -ArgumentList "/i `"$out`" /qn /norestart" -Wait
                break
            }
        } catch { Say "download attempt $i failed: $_"; Start-Sleep -Seconds 20 }
    }
}

$svc = Get-Service sshd -ErrorAction SilentlyContinue
if ($svc) {
    Set-Service -Name sshd -StartupType Automatic
    Start-Service sshd
    Say "sshd: $((Get-Service sshd).Status)"
    if (-not (Get-NetFirewallRule -Name 'ocserve-ssh' -ErrorAction SilentlyContinue)) {
        New-NetFirewallRule -Name 'ocserve-ssh' -DisplayName 'ocserve VM ssh' `
            -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
    }
} else {
    Say 'ERROR: sshd not available; the VM cannot be driven'
}

# --- Authorized keys --------------------------------------------------------
# The Docker user is an Administrator, and Windows OpenSSH resolves admin keys
# from ProgramData rather than ~/.ssh. Both are written so either path works.
$pub = Get-Content 'C:\OEM\authorized_keys' -Raw
$adminKeys = 'C:\ProgramData\ssh\administrators_authorized_keys'
New-Item -ItemType Directory -Force -Path 'C:\ProgramData\ssh' | Out-Null
Set-Content -Path $adminKeys -Value $pub -Encoding ascii -NoNewline
icacls $adminKeys /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null
$userKeys = 'C:\Users\Docker\.ssh'
New-Item -ItemType Directory -Force -Path $userKeys | Out-Null
Set-Content -Path "$userKeys\authorized_keys" -Value $pub -Encoding ascii -NoNewline
icacls "$userKeys\authorized_keys" /inheritance:r /grant 'Docker:F' /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null
Say 'authorized_keys installed'

# --- Host tuning for a test box --------------------------------------------
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0
powercfg /change hibernate-timeout-ac 0
# No surprise reboots mid-battery.
New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' -Force | Out-Null
Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' -Name NoAutoRebootWithLoggedOnUsers -Value 1
# Long paths are left at the Windows default (disabled) on purpose: Phase W must
# verify ocserve against a stock machine, not a tuned one.
New-Item -ItemType Directory -Force -Path 'C:\ocserve' | Out-Null

Say 'setup done'
