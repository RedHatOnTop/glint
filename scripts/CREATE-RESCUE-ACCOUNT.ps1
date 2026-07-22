# glide-shell rescue account (SHELL_DESIGN §7 ladder 6). RUN ELEVATED.
# Creates local admin 'glide-rescue'. The shell override is HKCU-scoped, so
# this account always boots stock explorer. You pick the password (user
# decision 0721: password is set by the user, never scripted).

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
        ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error "Run this elevated."
    exit 1
}
$pw = Read-Host -AsSecureString "Password for local account 'glide-rescue'"
New-LocalUser -Name glide-rescue -Password $pw -FullName "glide-shell rescue" `
    -Description "Stock-shell rescue account (glide-shell SHELL_DESIGN §7)" -ErrorAction Stop
# SID, not name: the admin group is 'Administrators' even on ko-KR, but the
# well-known SID is unconditional.
$adm = Get-LocalGroup -SID "S-1-5-32-544"
Add-LocalGroupMember -Group $adm -Member glide-rescue -ErrorAction Stop
Write-Host "glide-rescue created and added to $($adm.Name)."
