# Run a PowerShell command in the logged-on user's desktop session (SSH runs in session 0,
# where windows, hotkeys and the tray don't exist) and print its output.
#   ssh win-agent 'C:\mk\desktop.ps1 "C:\mk\mic.ps1 hotkey"'
param([Parameter(Mandatory)][string]$Command, [int]$TimeoutSec = 60)

$id = 'mk-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$job = "C:\mk\$id.ps1"
$out = "C:\mk\$id.txt"
Set-Content -Path $job -Value $Command -Encoding UTF8

$arg = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command `"& '$job' *>&1 | Out-File -Encoding utf8 '$out'`""
$action = New-ScheduledTaskAction -Execute powershell.exe -Argument $arg
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName $id -Action $action -Principal $principal -Force | Out-Null
Start-ScheduledTask -TaskName $id

$deadline = (Get-Date).AddSeconds($TimeoutSec)
do {
    Start-Sleep -Milliseconds 300
    $info = Get-ScheduledTaskInfo -TaskName $id
    $done = $info.LastRunTime -gt [datetime]'2000-01-01' -and (Get-ScheduledTask -TaskName $id).State -ne 'Running'
} until ($done -or (Get-Date) -gt $deadline)
Unregister-ScheduledTask -TaskName $id -Confirm:$false
if (Test-Path $out) { Get-Content $out; Remove-Item $out }
Remove-Item $job
