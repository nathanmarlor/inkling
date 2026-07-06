#!/usr/bin/env bash
# Deploy scribefb (hook-free page-clear xovi extension) + the scribed daemon, then
# self-test the page-clear trigger.
#
# Usage:  RM2_HOST=<tablet-ip> RM2_PASS='<root-password>' ./deploy.sh
set -euo pipefail

HOST="${RM2_HOST:?set RM2_HOST}"
PASS="${RM2_PASS:?set RM2_PASS}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT="$ROOT/xovi-ext/scribefb/scribefb.so"
BIN="$ROOT/scribed/target/armv7-unknown-linux-musleabihf/release/scribed"

pass_file="$(mktemp)"; trap 'rm -f "$pass_file"' EXIT
printf '%s\n' "$PASS" > "$pass_file"
ssh_() { sshpass -f "$pass_file" ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 "root@${HOST}" "$@"; }
scp_() { sshpass -f "$pass_file" scp -o StrictHostKeyChecking=accept-new "$1" "root@${HOST}:$2"; }

echo "== stop scribed + xochitl =="
ssh_ 'systemctl stop scribed 2>/dev/null; systemctl stop xochitl'

echo "== copy extension + daemon =="
scp_ "$EXT" /home/root/xovi/extensions.d/scribefb.so
scp_ "$BIN" /home/root/scribed

echo "== re-apply the tmpfs LD_PRELOAD drop-in (wiped on reboot) =="
ssh_ 'mkdir -p /etc/systemd/system/xochitl.service.d && cat > /etc/systemd/system/xochitl.service.d/xovi.conf <<EOF
[Service]
Environment="LD_PRELOAD=/home/root/xovi/xovi.so"
Environment="XOVI_ROOT=/home/root/xovi"
EOF
systemctl daemon-reload'

echo "== start xochitl (xovi auto-loads scribefb) =="
ssh_ 'systemctl start xochitl'
sleep 8

echo "== confirm scribefb loaded =="
ssh_ 'journalctl -u xochitl --no-pager -n 200 | grep -i scribefb | tail -5 || echo "(no scribefb log yet)"'

echo "== done. start the daemon with:  ssh root@${HOST} systemctl start scribed =="
echo "   (page-clear self-test: ssh root@${HOST} touch /tmp/scribefb_clear — clears the OPEN page)"
