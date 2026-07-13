#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
  echo "install-service.sh must be run as root" >&2
  exit 1
fi

SOURCE_DIR=${1:-$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)}
INSTALL_DIR=/opt/rsduck
STATE_DIR=/var/lib/rsduck
UNIT_PATH=/etc/systemd/system/rsduck.service

for required in rsduck rsduck-tray rsduck.toml.default init.sql.default rsduck.service rsduck-tray.desktop; do
  if [ ! -f "$SOURCE_DIR/$required" ]; then
    echo "missing package file: $SOURCE_DIR/$required" >&2
    exit 1
  fi
done
if [ ! -d "$SOURCE_DIR/extensions" ]; then
  echo "missing package directory: $SOURCE_DIR/extensions" >&2
  exit 1
fi

if ! id rsduck >/dev/null 2>&1; then
  useradd --system --home-dir "$STATE_DIR" --shell /usr/sbin/nologin rsduck
fi

if systemctl is-active --quiet rsduck.service; then
  systemctl stop rsduck.service
fi

install -d -m 0755 "$INSTALL_DIR" "$STATE_DIR" "$STATE_DIR/logs" "$STATE_DIR/snapshot" "$STATE_DIR/extensions" /etc/xdg/autostart
install -m 0755 "$SOURCE_DIR/rsduck" "$INSTALL_DIR/rsduck"
install -m 0755 "$SOURCE_DIR/rsduck-tray" "$INSTALL_DIR/rsduck-tray"
install -m 0644 "$SOURCE_DIR/rsduck.service" "$UNIT_PATH"
install -m 0644 "$SOURCE_DIR/rsduck-tray.desktop" /etc/xdg/autostart/rsduck-tray.desktop
cp -R "$SOURCE_DIR/extensions/." "$STATE_DIR/extensions/"

if [ ! -f "$STATE_DIR/rsduck.toml" ]; then
  install -m 0644 "$SOURCE_DIR/rsduck.toml.default" "$STATE_DIR/rsduck.toml"
fi
if [ ! -f "$STATE_DIR/init.sql" ]; then
  install -m 0644 "$SOURCE_DIR/init.sql.default" "$STATE_DIR/init.sql"
fi

chown -R rsduck:rsduck "$STATE_DIR"
systemctl daemon-reload
systemctl enable rsduck.service
systemctl restart rsduck.service
systemctl is-active --quiet rsduck.service
