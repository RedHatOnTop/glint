# glide-shell rescue (SHELL_DESIGN §7 ladder 5).
# Removes the per-user shell override and starts explorer. Run from
# Ctrl+Alt+Del > Task Manager > Run new task, or any working session.
# No elevation needed — the override is HKCU.

reg delete "HKCU\Software\Microsoft\Windows NT\CurrentVersion\Winlogon" /v Shell /f 2>$null
Start-Process explorer.exe
Write-Host "HKCU Shell= removed; explorer started. Log off and back on for a clean stock session."
