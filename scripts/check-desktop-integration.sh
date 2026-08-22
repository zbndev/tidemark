#!/bin/sh
set -eu

project_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_root"

desktop_file=data/applications/io.github.zbndev.Tidemark.desktop
autostart_file=data/autostart/io.github.zbndev.Tidemark.desktop
metainfo_file=data/metainfo/io.github.zbndev.Tidemark.metainfo.xml
service_file=data/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service

desktop-file-validate "$desktop_file" "$autostart_file"
appstreamcli validate --pedantic --no-net "$metainfo_file"

grep -Fxq 'Name=io.github.zbndev.Tidemark.Daemon' "$service_file"
grep -Fxq 'Exec=/usr/bin/tidemarkd' "$service_file"
grep -Fxq 'SystemdService=tidemarkd.service' "$service_file"

for size in 16 22 24 32 48 64 128 256 512; do
    icon="data/icons/hicolor/${size}x${size}/apps/io.github.zbndev.Tidemark.png"
    file "$icon" | grep -Fq "PNG image data, ${size} x ${size},"
done

hidpi_icon=data/icons/hicolor/512x512@2/apps/io.github.zbndev.Tidemark.png
file "$hidpi_icon" | grep -Fq 'PNG image data, 1024 x 1024,'
