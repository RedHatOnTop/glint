# Build the Hyper-V VM that the M7 swap rehearsal runs in (SHELL_DESIGN §9).
#
# The rehearsal needs a machine where Winlogon\Shell can be flipped, the shell
# can be crash-looped on purpose to exercise the safety ladder, and the whole
# thing can be reverted in seconds. That is a VM with checkpoints, not this box.
#
# Edition matters: Shell Launcher (the supported way to run a custom shell,
# instead of the HKLM Winlogon override) only exists on Enterprise / Education /
# IoT Enterprise. Pro does not have it — hence an LTSC image.
#
# Run once, elevated. Everything after this is driven by MANAGE-LAB-VM.ps1.

param(
    [string]$Iso  = "$env:USERPROFILE\Downloads\ko-kr_windows_11_enterprise_ltsc_2024_x64_dvd_b6b6eb18.iso",
    [string]$Name = 'glint-lab',
    [string]$Root = 'C:\VMs',
    [int]$DiskGB  = 80,
    [int]$Cpus    = 4
)

$ErrorActionPreference = 'Stop'

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this from an elevated PowerShell — Hyper-V VM creation needs it.'
}
if (-not (Test-Path $Iso)) { throw "ISO not found: $Iso" }
if (Get-VM -Name $Name -ErrorAction SilentlyContinue) { throw "VM '$Name' already exists — remove it first, or pass -Name." }

$dir = Join-Path $Root $Name
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$vhd = Join-Path $dir "$Name.vhdx"

# Generation 2 = UEFI + Secure Boot, both of which Windows 11 requires.
New-VM -Name $Name -Generation 2 -MemoryStartupBytes 4GB `
       -NewVHDPath $vhd -NewVHDSizeBytes ($DiskGB * 1GB) -Path $dir | Out-Null

# Host has 16 GB and the build already saturates it. Let the VM give memory
# back when it is idle rather than pinning 4 GB while cargo is running.
Set-VMMemory    $Name -DynamicMemoryEnabled $true -MinimumBytes 2GB -MaximumBytes 6GB
Set-VMProcessor $Name -Count $Cpus

# Windows 11 also requires TPM 2.0, and a vTPM needs a key protector first.
# -NewLocalKeyProtector does it in one call; the New-HgsGuardian route works
# too but writes certificates into the host's store for no benefit here.
Set-VMKeyProtector -VMName $Name -NewLocalKeyProtector
Enable-VMTPM -VMName $Name

# Standard checkpoints capture running state. Production checkpoints quiesce
# the guest through VSS and come back at a login screen, which loses exactly
# the thing a shell rehearsal wants to compare against. Automatic checkpoints
# off so a revert always lands where we put it.
Set-VM $Name -CheckpointType Standard -AutomaticCheckpointsEnabled $false

# Guest Service Interface lets Copy-VMFile push a fresh glide-shell.exe into
# the VM with no network share and no logged-in session. Matched by its
# well-known component GUID, not by name — integration service names come back
# localized, and this host is ko-KR.
$gsi = Get-VMIntegrationService -VMName $Name | Where-Object { $_.Id -match '6C09BB55-D683-4DA0-8931-C9BF705F6A00' }
if ($gsi) { Enable-VMIntegrationService -VMIntegrationService $gsi }
else      { Write-Warning 'Guest Service Interface not found — Copy-VMFile will not work; use PowerShell Direct instead.' }

$sw = Get-VMSwitch -Name 'Default Switch' -ErrorAction SilentlyContinue
if ($sw) { Connect-VMNetworkAdapter -VMName $Name -SwitchName $sw.Name }
else     { Write-Warning "No 'Default Switch' — the VM has no network adapter attached." }

Add-VMDvdDrive -VMName $Name -Path $Iso
Set-VMFirmware $Name -FirstBootDevice (Get-VMDvdDrive -VMName $Name) `
               -EnableSecureBoot On -SecureBootTemplate MicrosoftWindows

Get-VM $Name | Format-List Name, State, Generation, ProcessorCount, MemoryStartup, CheckpointType

Write-Host @"

Created. Next:

  Start-VM $Name; vmconnect.exe localhost $Name

Walk OOBE once. Enterprise LTSC offers a local account — take the domain-join
path rather than signing in, so the lab has no account tied to anything.
Then checkpoint the clean state:

  Checkpoint-VM -Name $Name -SnapshotName 'clean'

From there, .\MANAGE-LAB-VM.ps1 does the push / swap / revert loop.
"@
