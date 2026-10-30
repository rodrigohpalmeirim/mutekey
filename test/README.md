# Test helpers

mutekey has no automated tests; it was checked by hand in a Windows 11 VM
([dockur/windows](https://github.com/dockur/windows)) reachable as `ssh win-agent`.
These scripts drive that VM from Linux:

| Script | What it does |
|---|---|
| `vm-setup.sh` | Prepares a fresh VM: allows scripts, copies the helpers and the release build to `C:\mk`, installs VB-Cable as a microphone. |
| `vmdesk.sh` | Runs PowerShell read from stdin in the logged-on desktop session (SSH runs in session 0, where there is no desktop, tray or hotkey). |
| `desktop.ps1` | The VM side of `vmdesk.sh`: a one-shot interactive scheduled task. |
| `mic.ps1` | Reads and sets microphone mute state directly, presses keys, clicks, scrolls, and reports the badge window. |
| `fg.ps1` | Prints the foreground window, to check that mutekey never steals focus. |
| `install-vbcable.ps1` | Installs [VB-Cable](https://vb-audio.com/Cable/) unattended. |
| `screendump.sh` | Saves the VM screen as a PNG through the QEMU monitor. |

Example:

```bash
test/vm-setup.sh ~/Downloads/VBCABLE_Driver_Pack45
test/vmdesk.sh <<'PS'
Start-Process C:\mk\mutekey.exe; Start-Sleep 2
C:\mk\mic.ps1 hotkey; Start-Sleep 1
C:\mk\mic.ps1 status; C:\mk\mic.ps1 badge
PS
test/screendump.sh /tmp/screen.png
```
