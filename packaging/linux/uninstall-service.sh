#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
  echo "uninstall-service.sh must be run as root" >&2
  exit 1
fi

if systemctl is-active --quiet rsduck.service; then
  systemctl stop rsduck.service
fi
if systemctl is-enabled --quiet rsduck.service; then
  systemctl disable rsduck.service
fi

rm -f /etc/systemd/system/rsduck.service
rm -f /etc/xdg/autostart/rsduck-tray.desktop
rm -rf /opt/rsduck
systemctl daemon-reload

echo "RSDuck service was removed. Runtime data remains in /var/lib/rsduck."
