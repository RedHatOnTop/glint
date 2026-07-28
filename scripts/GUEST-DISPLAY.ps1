# Change the lab guest's display mode, from *inside* the guest.
#
# Hyper-V's synthetic video only takes a resolution while the VM is off, and
# the desktop's display-change path (one window per monitor, refitted on
# WM_DISPLAYCHANGE) has to be driven on a live shell. ChangeDisplaySettings
# does it without a reboot — push this with
# MANAGE-LAB-VM.ps1 -Action push -Exe scripts\GUEST-DISPLAY.ps1 and run:
#
#   powershell -ep bypass -f C:\glint\GUEST-DISPLAY.ps1 -W 800 -H 600
#
# Without -W/-H it goes back to whatever the registry has for the display,
# which is the way out if a mode leaves the guest unreadable.

param(
    [int]$W,
    [int]$H
)

$ErrorActionPreference = 'Stop'

# Single-quoted: a double-quoted here-string would read the backticks and
# dollar signs in the C# below as PowerShell escapes.
Add-Type @'
using System;
using System.Runtime.InteropServices;

[StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
public struct DEVMODE {
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
    public ushort dmSpecVersion, dmDriverVersion, dmSize, dmDriverExtra;
    public uint dmFields;
    public int dmPositionX, dmPositionY;
    public uint dmDisplayOrientation, dmDisplayFixedOutput;
    public short dmColor, dmDuplex, dmYResolution, dmTTOption, dmCollate;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
    public ushort dmLogPixels;
    public uint dmBitsPerPel, dmPelsWidth, dmPelsHeight, dmDisplayFlags, dmDisplayFrequency;
    public uint dmICMMethod, dmICMIntent, dmMediaType, dmDitherType;
    public uint dmReserved1, dmReserved2, dmPanningWidth, dmPanningHeight;
}

public class Disp {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern bool EnumDisplaySettings(string device, int mode, ref DEVMODE dm);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int ChangeDisplaySettings(ref DEVMODE dm, uint flags);
    // The same call with no mode at all, which is what asks for the one in
    // the registry back. A `ref` parameter cannot carry NULL.
    [DllImport("user32.dll", EntryPoint = "ChangeDisplaySettingsW", CharSet = CharSet.Unicode)]
    public static extern int ChangeDisplaySettingsDefault(IntPtr dm, uint flags);
}
'@

# ENUM_CURRENT_SETTINGS
$dm = New-Object DEVMODE
$dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf([type][DEVMODE])
[Disp]::EnumDisplaySettings($null, -1, [ref]$dm) | Out-Null
Write-Host "now $($dm.dmPelsWidth)x$($dm.dmPelsHeight)"

if ($W -and $H) {
    $dm.dmPelsWidth = $W
    $dm.dmPelsHeight = $H
    # DM_PELSWIDTH | DM_PELSHEIGHT
    $dm.dmFields = 0x80000 -bor 0x100000
    # DISP_CHANGE_SUCCESSFUL is 0; -2 means the mode is not supported.
    $r = [Disp]::ChangeDisplaySettings([ref]$dm, 0)
    Write-Host "change to ${W}x${H} -> $r"
}
else {
    $r = [Disp]::ChangeDisplaySettingsDefault([IntPtr]::Zero, 0)
    Write-Host "reset -> $r"
}
