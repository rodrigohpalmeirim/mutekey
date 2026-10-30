#!/usr/bin/env bash
# Run the PowerShell read from stdin in the VM's desktop session and print its output.
set -euo pipefail
job=/tmp/mk-job-$$.ps1
cat > "$job"
scp -q "$job" win-agent:C:/mk/job.ps1
rm -f "$job"
ssh win-agent "C:\\mk\\desktop.ps1 'C:\\mk\\job.ps1' ${1:-60}"
