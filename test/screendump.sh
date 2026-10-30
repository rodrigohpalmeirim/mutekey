#!/usr/bin/env bash
# Save the VM's screen as a PNG via the QEMU monitor (works even when SSH/MCP are down).
set -euo pipefail
out=${1:-/tmp/mk/vm.png}
docker exec win-agent sh -c 'echo "screendump /tmp/x.ppm" | nc -U -q1 /run/shm/monitor.sock >/dev/null; sleep 1'
docker cp -q win-agent:/tmp/x.ppm /tmp/mk-x.ppm
python3 -c "import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[2])" /tmp/mk-x.ppm "$out"
echo "$out"
