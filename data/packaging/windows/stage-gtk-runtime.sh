#!/usr/bin/env bash
# Stage the GTK 4 / libadwaita runtime for the NSIS installer (todo 21).
#
# Downloads the SAME pinned gvsbuild bundle the windows-ucrt64-probe /
# windows-msvc-probe CI jobs pin (tag 2026.8.0) and assembles a lean runtime
# tree under build/nsis-staging/gtk/: the bundle's bin/*.dll, the gdk-pixbuf
# SVG loader with a loaders.cache pointing at relative install paths, glib
# schemas, icon themes and fontconfig data. Dev-only content (pdbs, tools,
# introspection, docs, locales) is excluded.
#
# Usage: data/packaging/windows/stage-gtk-runtime.sh [workdir]
# Requires: curl, unzip, a UCRT64 shell for nothing (the cache generation
# runs the bundle's own query tool, no toolchain needed).
set -euo pipefail

GVBUILD_TAG='2026.8.0'
GVBUILD_URL="https://github.com/wingtk/gvsbuild/releases/download/${GVBUILD_TAG}/GTK4_Gvsbuild_${GVBUILD_TAG}_x64.zip"

WORK="${1:-build}"
SRC="$WORK/gvsbuild-extract"
DST="$WORK/nsis-staging/gtk"

mkdir -p "$WORK" "$DST"

if [ ! -f "$WORK/gvsbuild.zip" ]; then
  echo "downloading $GVBUILD_URL"
  curl --fail -L -o "$WORK/gvsbuild.zip" "$GVBUILD_URL"
fi

rm -rf "$SRC"
mkdir -p "$SRC"
unzip -q -o "$WORK/gvsbuild.zip" -d "$SRC"
SRC="$(cd "$SRC" && pwd)"   # absolute: the script cd's into DST later

# Runtime DLLs only, at the prefix root: they must sit next to tidemark.exe,
# which is also what makes gdk-pixbuf/glib resolve their lib/ and share/ data
# relative to the install prefix. The bundle's exe tools and pdb symbols never
# ship.
rm -rf "$DST/bin"
mkdir -p "$DST"
cp "$SRC"/bin/*.dll "$DST/"

# gdk-pixbuf loaders + a loaders.cache whose paths are relative to the GTK
# install root, so they resolve under %LOCALAPPDATA%\Programs\tidemark\gtk.
rm -rf "$DST/lib" "$DST/share"
mkdir -p "$DST/lib/gdk-pixbuf-2.0/2.10.0/loaders"
mkdir -p "$DST/share/glib-2.0/schemas" "$DST/share/icons"
cp "$SRC"/lib/gdk-pixbuf-2.0/2.10.0/loaders/*.dll "$DST/lib/gdk-pixbuf-2.0/2.10.0/loaders/"
cp -r "$SRC"/share/glib-2.0/schemas/. "$DST/share/glib-2.0/schemas/"
cp -r "$SRC"/share/icons/Adwaita "$DST/share/icons/"
cp -r "$SRC"/share/icons/hicolor "$DST/share/icons/"
cp -r "$SRC"/share/fontconfig "$DST/share/" 2>/dev/null || true

# The bundle ships gchemas.compiled already; keep it and regenerate the pixbuf
# cache with paths relative to the install root (the query tool emits
# absolute paths, so the staging-root prefix is stripped afterwards).
DST_ABS="$(cygpath -m "$(cd "$DST" && pwd)")"
CACHE="lib/gdk-pixbuf-2.0/2.10.0/loaders/loaders.cache"
"$SRC/bin/gdk-pixbuf-query-loaders.exe" \
  "$DST_ABS/lib/gdk-pixbuf-2.0/2.10.0/loaders/pixbufloader_svg.dll" \
  > "$DST/$CACHE"
sed -i "s|^\"$DST_ABS/|\"|" "$DST/$CACHE"
grep -q '^"lib/' "$DST/$CACHE" || {
  echo "loaders.cache did not end up relative" >&2; exit 1
}

echo "staged runtime:"; du -sh "$DST_ABS"
