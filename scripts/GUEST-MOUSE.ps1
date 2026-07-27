# Pointer injection from *inside* the lab guest.
#
# Hyper-V exposes a synthetic keyboard over WMI (VM-CONSOLE.ps1) but nothing
# for the pointer, and the desktop is a mouse surface: right-click menus,
# double-click open, marquee select. So the click is made locally instead —
# push this with MANAGE-LAB-VM.ps1 -Action push -Exe scripts\GUEST-MOUSE.ps1
# and drive it from the guest console:
#
#   powershell -ep bypass -f C:\glint\GUEST-MOUSE.ps1 -X 62 -Y 45 -Click right
#
# Coordinates are physical pixels, origin top-left, same as the thumbnails
# VM-CONSOLE -Action shot writes.

param(
    [Parameter(Mandatory)][int]$X,
    [Parameter(Mandatory)][int]$Y,
    [ValidateSet('move', 'left', 'right', 'double')][string]$Click = 'move',
    # Held before the click and released after: 17 Ctrl, 16 Shift.
    [int[]]$Hold
)

$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class GuestMouse {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, IntPtr extra);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
}
"@

$LDOWN = 0x0002; $LUP = 0x0004; $RDOWN = 0x0008; $RUP = 0x0010
$KEYUP = 0x0002

function Tap([int]$down, [int]$up) {
    [GuestMouse]::mouse_event($down, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 40
    [GuestMouse]::mouse_event($up, 0, 0, 0, [IntPtr]::Zero)
}

[GuestMouse]::SetCursorPos($X, $Y) | Out-Null
Start-Sleep -Milliseconds 150

foreach ($vk in $Hold) { [GuestMouse]::keybd_event([byte]$vk, 0, 0, [IntPtr]::Zero) }
try {
    switch ($Click) {
        'left'  { Tap $LDOWN $LUP }
        'right' { Tap $RDOWN $RUP }
        'double' {
            Tap $LDOWN $LUP
            # Inside the double-click time, which defaults to 500ms.
            Start-Sleep -Milliseconds 90
            Tap $LDOWN $LUP
        }
    }
}
finally {
    # Release even if the click threw, or the guest is left with a modifier
    # stuck down and every later keystroke is a chord.
    foreach ($vk in $Hold) { [GuestMouse]::keybd_event([byte]$vk, 0, $KEYUP, [IntPtr]::Zero) }
}

Write-Host "$Click at $X,$Y"
