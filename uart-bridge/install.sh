#!/usr/bin/env bash
# Install the oven UART bridge on the Pi. Run from this directory on the Pi:
#   sudo ./install.sh
# Assumes the serial port is already enabled with no login console on it
# (raspi-config: Interface Options -> Serial Port, "No" then "Yes").

set -euo pipefail

cd "$(dirname "$0")"

apt-get install -y ser2net socat tio

install -m 0644 ser2net.yaml /etc/ser2net.yaml
install -d /etc/systemd/journald.conf.d
install -m 0644 journald-oven.conf /etc/systemd/journald.conf.d/oven.conf
install -m 0644 oven-uart-log.service /etc/systemd/system/oven-uart-log.service

systemctl restart systemd-journald
systemctl daemon-reload
systemctl enable ser2net oven-uart-log
systemctl restart ser2net oven-uart-log

echo "Installed. Follow the console with: journalctl -u oven-uart-log -f"
