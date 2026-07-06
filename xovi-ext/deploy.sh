#!/usr/bin/env bash
# Deploy inklingfb (hook-free page-clear xovi extension) + the inkling daemon, then
# self-test the page-clear trigger.
#
# Usage:  RM2_HOST=<tablet-ip> RM2_PASS='<root-password>' ./deploy.sh
set -euo pipefail

HOST="${RM2_HOST:?set RM2_HOST}"
PASS="${RM2_PASS:?set RM2_PASS}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT="$ROOT/xovi-ext/inklingfb/inklingfb.so"
BIN="$ROOT/daemon/target/armv7-unknown-linux-musleabihf/release/inkling"

pass_file="$(mktemp)"; trap 'rm -f "$pass_file"' EXIT
printf '%s\n' "$PASS" > "$pass_file"
ssh_() { sshpass -f "$pass_file" ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 "root@${HOST}" "$@"; }
scp_() { sshpass -f "$pass_file" scp -o StrictHostKeyChecking=accept-new "$1" "root@${HOST}:$2"; }

echo "== stop inkling + xochitl =="
ssh_ 'systemctl stop inkling 2>/dev/null; systemctl stop xochitl'

echo "== copy extension + daemon =="
scp_ "$EXT" /home/root/xovi/extensions.d/inklingfb.so
scp_ "$BIN" /home/root/inkling

echo "== re-apply the tmpfs LD_PRELOAD drop-in (wiped on reboot) =="
ssh_ 'mkdir -p /etc/systemd/system/xochitl.service.d && cat > /etc/systemd/system/xochitl.service.d/xovi.conf <<EOF
[Service]
Environment="LD_PRELOAD=/home/root/xovi/xovi.so"
Environment="XOVI_ROOT=/home/root/xovi"
EOF
systemctl daemon-reload'

echo "== start xochitl (xovi auto-loads inklingfb) =="
ssh_ 'systemctl start xochitl'
sleep 8

echo "== confirm inklingfb loaded =="
ssh_ 'journalctl -u xochitl --no-pager -n 200 | grep -i inklingfb | tail -5 || echo "(no inklingfb log yet)"'

echo "== done. start the daemon with:  ssh root@${HOST} systemctl start inkling =="
echo "   (page-clear self-test: ssh root@${HOST} touch /tmp/inklingfb_clear — clears the OPEN page)"
