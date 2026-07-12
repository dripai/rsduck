#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
  echo "uninstall-service.sh must be run as root" >&2
  exit 1
fi

launchctl bootout system/com.dripai.rsduck >/dev/null 2>&1 || true
rm -f /Library/LaunchDaemons/com.dripai.rsduck.plist
rm -f /Library/LaunchAgents/com.dripai.rsduck-tray.plist
rm -rf "/Library/Application Support/rsduck"

echo "RSDuck service and runtime data were removed."
