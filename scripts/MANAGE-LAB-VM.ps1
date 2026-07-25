# Drive the M7 rehearsal loop in the lab VM created by NEW-LAB-VM.ps1.
#
# The loop is: revert to a known checkpoint, push a fresh build in, flip the
# shell, reboot, look. Everything here is scripted because the interesting part
# is the twentieth iteration, not the first.
#
# 'exec' and 'pull' go over PowerShell Direct — a VMBus session that needs no
# network, no RDP and no working shell in the guest. That is the whole reason
# this lab is on Hyper-V: once Winlogon\Shell points at a binary that crashes,
# VMBus is the only way left in.
#
# Elevated, like every Hyper-V cmdlet — unless the account is in the local
# Hyper-V Administrators group.

param(
    [Parameter(Mandatory)]
    [ValidateSet('status', 'start', 'stop', 'push', 'exec', 'pull', 'checkpoint', 'revert', 'checkpoints', 'setcred')]
    [string]$Action,
    [string]$Name = 'glint-lab',
    # checkpoint / revert target.
    [string]$Snapshot = 'clean',
    # push source; the release build once there is one.
    [string]$Exe = "$PSScriptRoot\..\target\debug\glide-shell.exe",
    [string]$GuestDir = 'C:\glint',
    # exec: PowerShell to run inside the guest.
    [string]$Command,
    # pull: guest path to copy out, and where to put it on the host.
    [string]$GuestPath,
    [string]$Out
)

$ErrorActionPreference = 'Stop'

# Guest credentials for PowerShell Direct, stashed once by -Action setcred.
# Export-Clixml encrypts under DPAPI for this user on this machine, so the file
# is useless if it leaves the box. Gitignored regardless.
$CredPath = Join-Path $PSScriptRoot 'lab-cred.xml'

function Get-Lab {
    $vm = Get-VM -Name $Name -ErrorAction SilentlyContinue
    if (-not $vm) { throw "VM '$Name' not found — run NEW-LAB-VM.ps1 first." }
    $vm
}

function Get-LabCred {
    if (-not (Test-Path $CredPath)) { throw "No saved guest credential — run: .\MANAGE-LAB-VM.ps1 -Action setcred" }
    Import-Clixml $CredPath
}

switch ($Action) {
    'setcred' {
        # Interactive on purpose; run it yourself once.
        Get-Credential -Message "Local account inside the '$Name' guest" | Export-Clixml $CredPath
        Write-Host "saved -> $CredPath"
    }

    'status' {
        $vm = Get-Lab
        $vm | Format-List Name, State, Uptime, CPUUsage, MemoryAssigned, Status
        Get-VMSnapshot -VMName $Name | Select-Object Name, SnapshotType, CreationTime | Format-Table -Auto
    }

    'start' {
        Start-VM -Name $Name
        Write-Host "started. console:  vmconnect.exe localhost $Name"
    }

    'stop' {
        # Graceful first — a hard stop mid-write is how a lab VM acquires a
        # corrupt profile and starts lying about what the shell did.
        Stop-VM -Name $Name
    }

    'push' {
        if (-not (Test-Path $Exe)) { throw "not built: $Exe" }
        $vm = Get-Lab
        if ($vm.State -ne 'Running') { throw "VM is $($vm.State) — 'push' needs it running with integration services up." }
        $dest = Join-Path $GuestDir (Split-Path $Exe -Leaf)
        Copy-VMFile -Name $Name -SourcePath $Exe -DestinationPath $dest `
                    -FileSource Host -CreateFullPath -Force
        Write-Host "pushed  ->  $dest   ($([math]::Round((Get-Item $Exe).Length/1MB,1)) MB)"
        Write-Host @"

In the guest, make it the shell for that user and log off:

  reg add "HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon" /v Shell /t REG_SZ /d "$dest" /f

To undo from inside a broken session: Ctrl+Alt+Del > 작업 관리자 > 새 작업 실행
> the RESTORE-SHELL.ps1 line. From outside: revert the checkpoint, or

  .\MANAGE-LAB-VM.ps1 -Action exec -Command 'reg delete "HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon" /v Shell /f'
"@
    }

    'exec' {
        if (-not $Command) { throw "-Command is required for 'exec'." }
        Invoke-Command -VMName $Name -Credential (Get-LabCred) -ScriptBlock ([scriptblock]::Create($Command))
    }

    'pull' {
        if (-not $GuestPath) { throw "-GuestPath is required for 'pull'." }
        if (-not $Out) { $Out = Join-Path (Get-Location) (Split-Path $GuestPath -Leaf) }
        # Copy-VMFile is host-to-guest only, so the way back out is a session.
        $s = New-PSSession -VMName $Name -Credential (Get-LabCred)
        try   { Copy-Item -FromSession $s -Path $GuestPath -Destination $Out -Force }
        finally { Remove-PSSession $s }
        Write-Host "pulled  $GuestPath  ->  $Out"
    }

    'checkpoint' {
        Checkpoint-VM -Name $Name -SnapshotName $Snapshot
        Write-Host "checkpoint '$Snapshot' taken."
    }

    'revert' {
        $snap = Get-VMSnapshot -VMName $Name -Name $Snapshot -ErrorAction SilentlyContinue
        if (-not $snap) { throw "no checkpoint named '$Snapshot' — see: -Action checkpoints" }
        Restore-VMSnapshot -VMSnapshot $snap -Confirm:$false
        Write-Host "reverted to '$Snapshot'."
    }

    'checkpoints' {
        Get-VMSnapshot -VMName $Name | Select-Object Name, SnapshotType, CreationTime, ParentSnapshotName | Format-Table -Auto
    }
}
