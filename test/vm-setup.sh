#!/usr/bin/env bash
# Prepare a freshly restored win-agent VM for testing mutekey: allow scripts, copy the test
# helpers and the release build to C:\mk, and install VB-Cable as a microphone.
#   test/vm-setup.sh /path/to/extracted/VBCABLE_Driver_Pack
set -euo pipefail
cd "$(dirname "$0")/.."
vbcable=${1:?usage: test/vm-setup.sh VBCABLE_DIR (the extracted VBCABLE_Driver_Pack45.zip)}
ssh win-agent 'Set-ExecutionPolicy -Scope LocalMachine Bypass -Force; New-Item -ItemType Directory -Force C:\mk | Out-Null'
scp -q test/*.ps1 target/x86_64-pc-windows-gnu/release/mutekey.exe win-agent:C:/mk/
ssh win-agent 'Test-Path C:\mk\vbcable\VBCABLE_Setup_x64.exe' | grep -q True || scp -q -r "$vbcable" win-agent:C:/mk/vbcable
ssh win-agent 'C:\mk\install-vbcable.ps1 | Out-Null; C:\mk\mic.ps1 status'
