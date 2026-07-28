# Read and drive a Hyper-V VM's console with no cooperation from the guest.
#
# PowerShell Direct needs a booted guest with an account to log into. This does
# not: it goes at the virtualization stack itself, so it works in the installer,
# at the logon screen, and — the case this lab exists for — when Winlogon\Shell
# points at a binary that does not come up. If the shell is dead this is the
# only thing that can still answer "what is on screen".
#
# Thumbnails come back as RGB565 and the platform clamps the size, so this is
# for reading a screen, not for judging how it looks. Full-resolution captures
# go through MANAGE-LAB-VM.ps1 -Action pull once the guest is alive.
#
# Elevated, like everything else that touches Hyper-V.

param(
    [Parameter(Mandatory)]
    [ValidateSet('shot', 'type', 'keytext', 'key', 'keys', 'chord')]
    [string]$Action,
    [string]$Name = 'glint-lab',
    [string]$Out = 'vm-console.png',
    [int]$Width = 1024,
    [int]$Height = 768,
    # type: literal text to send.
    [string]$Text,
    # key / keys: virtual-key codes. 13 = Enter, 32 = Space, 27 = Esc, 9 = Tab.
    [int[]]$KeyCode
)

$ErrorActionPreference = 'Stop'
$ns = 'root\virtualization\v2'

$vm = Get-CimInstance -Namespace $ns -ClassName Msvm_ComputerSystem -Filter "ElementName='$Name'"
if (-not $vm) { throw "VM '$Name' not found in $ns — wrong name, or this session is not elevated." }

switch ($Action) {
    'shot' {
        # The thumbnail is taken against the realized settings of the VM, not
        # the VM object — the planned settings are a different instance and
        # produce a blank image.
        $vssd = Get-CimAssociatedInstance -InputObject $vm -ResultClassName Msvm_VirtualSystemSettingData |
                Where-Object { $_.VirtualSystemType -eq 'Microsoft:Hyper-V:System:Realized' }
        $svc = Get-CimInstance -Namespace $ns -ClassName Msvm_VirtualSystemManagementService

        $r = Invoke-CimMethod -InputObject $svc -MethodName GetVirtualSystemThumbnailImage -Arguments @{
            TargetSystem = [CimInstance]$vssd
            WidthPixels  = [uint16]$Width
            HeightPixels = [uint16]$Height
        }
        if ($r.ReturnValue -ne 0) { throw "GetVirtualSystemThumbnailImage failed: $($r.ReturnValue)" }
        if (-not $r.ImageData)    { throw "no image data — is the VM running?" }

        Add-Type -AssemblyName System.Drawing
        $bmp = New-Object System.Drawing.Bitmap($Width, $Height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
        $rect = New-Object System.Drawing.Rectangle(0, 0, $Width, $Height)
        $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::WriteOnly, $bmp.PixelFormat)
        try {
            $src = $r.ImageData
            $row = New-Object byte[] $data.Stride
            for ($y = 0; $y -lt $Height; $y++) {
                for ($x = 0; $x -lt $Width; $x++) {
                    # RGB565, little-endian, widened to 8 bits per channel by
                    # replicating the high bits so white stays white.
                    $i = ($y * $Width + $x) * 2
                    $v = [uint16]$src[$i] -bor ([uint16]$src[$i + 1] -shl 8)
                    $r5 = ($v -shr 11) -band 0x1F
                    $g6 = ($v -shr 5)  -band 0x3F
                    $b5 = $v           -band 0x1F
                    $o = $x * 3
                    $row[$o]     = [byte](($b5 -shl 3) -bor ($b5 -shr 2))
                    $row[$o + 1] = [byte](($g6 -shl 2) -bor ($g6 -shr 4))
                    $row[$o + 2] = [byte](($r5 -shl 3) -bor ($r5 -shr 2))
                }
                [System.Runtime.InteropServices.Marshal]::Copy($row, 0, [IntPtr]::Add($data.Scan0, $y * $data.Stride), $data.Stride)
            }
        }
        finally { $bmp.UnlockBits($data) }

        # Join-Path happily concatenates an already-rooted second element into
        # nonsense like C:\repo\C:/tmp/shot.png, which GetFullPath then rejects.
        if (-not [System.IO.Path]::IsPathRooted($Out)) { $Out = Join-Path (Get-Location) $Out }
        $Out = [System.IO.Path]::GetFullPath($Out)
        $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
        $bmp.Dispose()
        Write-Host "shot $Name ${Width}x${Height} -> $Out"
    }

    'type' {
        if (-not $Text) { throw '-Text is required for "type".' }
        $kbd = Get-CimAssociatedInstance -InputObject $vm -ResultClassName Msvm_Keyboard
        $r = Invoke-CimMethod -InputObject $kbd -MethodName TypeText -Arguments @{ asciiText = $Text }
        if ($r.ReturnValue -ne 0) { throw "TypeText failed: $($r.ReturnValue)" }
        Write-Host "typed $($Text.Length) chars"
    }

    'keytext' {
        # TypeText hands the string to the guest's input stack, which after a
        # reboot on this VM silently swallows every character while TypeKey
        # still lands — so type the string as virtual keys instead. US layout,
        # which is what the guest reports even with a Korean IME installed.
        if (-not $Text) { throw '-Text is required for "keytext".' }
        # Keys are strings throughout: a PowerShell hashtable literal indexed
        # with ' ' stores a [string], and a lookup with a [char] then misses.
        $plain = @{}
        foreach ($c in 'abcdefghijklmnopqrstuvwxyz'.ToCharArray()) { $plain["$c"] = [uint16][char]"$c".ToUpper() }
        foreach ($c in '0123456789'.ToCharArray()) { $plain["$c"] = [uint16][char]$c }
        $plain[' ']  = 32
        $plain[';']  = 0xBA; $plain['='] = 0xBB; $plain[','] = 0xBC; $plain['-'] = 0xBD
        $plain['.']  = 0xBE; $plain['/'] = 0xBF; $plain['`'] = 0xC0; $plain['['] = 0xDB
        $plain['\']  = 0xDC; $plain[']'] = 0xDD; $plain["'"] = 0xDE
        $shifted = @{
            ':' = 0xBA; '+' = 0xBB; '<' = 0xBC; '_' = 0xBD; '>' = 0xBE; '?' = 0xBF
            '~' = 0xC0; '{' = 0xDB; '|' = 0xDC; '}' = 0xDD; '"' = 0xDE
            '!' = 0x31; '@' = 0x32; '#' = 0x33; '$' = 0x34; '%' = 0x35
            '^' = 0x36; '&' = 0x37; '*' = 0x38; '(' = 0x39; ')' = 0x30
        }
        $kbd = Get-CimAssociatedInstance -InputObject $vm -ResultClassName Msvm_Keyboard
        $sent = 0
        foreach ($ch in $Text.ToCharArray()) {
            $c = [string]$ch
            $needShift = $false
            # Capitals are tested first: hashtable keys are case-insensitive,
            # so 'Y' finds the 'y' entry and would be typed without the shift.
            if ($c -cmatch '[A-Z]')              { $vk = [uint16][char]$c; $needShift = $true }
            elseif ($plain.ContainsKey($c))      { $vk = $plain[$c] }
            elseif ($shifted.ContainsKey($c))    { $vk = $shifted[$c]; $needShift = $true }
            else { throw "keytext cannot type '$c' — extend the table." }
            # TypeKey inside a held shift arrives out of order on this VM and
            # duplicates the run typed before it, so shifted characters go as an
            # explicit press/key/release with the guest given time between each.
            if ($needShift) {
                $null = Invoke-CimMethod -InputObject $kbd -MethodName PressKey -Arguments @{ keyCode = [uint16]16 }
                Start-Sleep -Milliseconds 60
            }
            $r = Invoke-CimMethod -InputObject $kbd -MethodName TypeKey -Arguments @{ keyCode = [uint16]$vk }
            if ($needShift) {
                Start-Sleep -Milliseconds 60
                $null = Invoke-CimMethod -InputObject $kbd -MethodName ReleaseKey -Arguments @{ keyCode = [uint16]16 }
                Start-Sleep -Milliseconds 60
            }
            if ($r.ReturnValue -ne 0) { throw "TypeKey for '$c' failed: $($r.ReturnValue)" }
            $sent++
            Start-Sleep -Milliseconds 30
        }
        Write-Host "keytyped $sent chars"
    }

    { $_ -in 'key', 'keys' } {
        if (-not $KeyCode) { throw '-KeyCode is required (13 Enter, 32 Space, 27 Esc, 9 Tab).' }
        $kbd = Get-CimAssociatedInstance -InputObject $vm -ResultClassName Msvm_Keyboard
        foreach ($k in $KeyCode) {
            $r = Invoke-CimMethod -InputObject $kbd -MethodName TypeKey -Arguments @{ keyCode = [uint16]$k }
            if ($r.ReturnValue -ne 0) { throw "TypeKey $k failed: $($r.ReturnValue)" }
            Start-Sleep -Milliseconds 80
        }
        Write-Host "sent $($KeyCode.Count) key(s)"
    }

    'chord' {
        # TypeKey is a tap, so it cannot express Alt+Tab — and Alt+Tab is the
        # only way back to a window once a shell restart has left the guest with
        # nothing focused and no mouse to click with. Hold the modifiers down,
        # tap the last key, let go in reverse. 18 = Alt, 17 = Ctrl, 16 = Shift.
        if (-not $KeyCode -or $KeyCode.Count -lt 2) { throw '-KeyCode needs at least two codes for "chord" (e.g. 18,9 for Alt+Tab).' }
        $kbd = Get-CimAssociatedInstance -InputObject $vm -ResultClassName Msvm_Keyboard
        $held = $KeyCode[0..($KeyCode.Count - 2)]
        foreach ($k in $held) {
            $r = Invoke-CimMethod -InputObject $kbd -MethodName PressKey -Arguments @{ keyCode = [uint16]$k }
            if ($r.ReturnValue -ne 0) { throw "PressKey $k failed: $($r.ReturnValue)" }
        }
        $last = $KeyCode[-1]
        $r = Invoke-CimMethod -InputObject $kbd -MethodName TypeKey -Arguments @{ keyCode = [uint16]$last }
        if ($r.ReturnValue -ne 0) { throw "TypeKey $last failed: $($r.ReturnValue)" }
        Start-Sleep -Milliseconds 120
        # Release even if the tap failed, or the guest is left with Alt stuck
        # down and every later keystroke becomes a menu accelerator.
        [array]::Reverse($held)
        foreach ($k in $held) {
            $r = Invoke-CimMethod -InputObject $kbd -MethodName ReleaseKey -Arguments @{ keyCode = [uint16]$k }
            if ($r.ReturnValue -ne 0) { throw "ReleaseKey $k failed: $($r.ReturnValue)" }
        }
        Write-Host "chord $($KeyCode -join '+')"
    }
}
