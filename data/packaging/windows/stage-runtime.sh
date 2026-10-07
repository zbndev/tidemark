#!/usr/bin/env bash
# Stage the MSYS2 UCRT64 runtime that the locked Windows GNU build links against.
#
# The DLL set is not hand-maintained: this script walks the PE import closure of
# tidemark.exe and tidemarkd.exe. The client draws with Slint and imports system DLLs
# only; what is staged is the daemon's system SQLite and whatever that pulls in. Package
# versions and package archive hashes are locked in msys2-runtime-packages.txt.
#
# Usage: data/packaging/windows/stage-runtime.sh [workdir] [release-dir]
# Requires an MSYS2 UCRT64 environment with objdump, pacman and the packages in
# msys2-runtime-packages.txt installed at exactly the recorded versions.
set -euo pipefail

WORK="${1:-build}"
RELEASE_DIR="${2:-target/release}"
PREFIX="${MINGW_PREFIX:-/ucrt64}"
SRC_BIN="$PREFIX/bin"
DST="$WORK/nsis-staging/runtime"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PACKAGE_LOCK="$SCRIPT_DIR/msys2-runtime-packages.txt"
IMPORT_LOG="$WORK/nsis-runtime-imports.txt"
DLL_LOG="$WORK/nsis-runtime-dlls.txt"
PACKAGE_LOG="$WORK/nsis-runtime-packages.txt"

for tool in objdump pacman cygpath; do
  command -v "$tool" >/dev/null || { echo "required tool not found: $tool" >&2; exit 1; }
done
for file in "$RELEASE_DIR/tidemark.exe" "$RELEASE_DIR/tidemarkd.exe" "$PACKAGE_LOCK" \
    "$SCRIPT_DIR/../../../data/icons/tidemark.ico"; do
  test -f "$file" || { echo "required input not found: $file" >&2; exit 1; }
done
mkdir -p "$WORK"

echo "verifying pinned MSYS2 runtime packages"
: > "$PACKAGE_LOG"
while read -r package version sha256; do
  [[ -n "${package:-}" && "${package:0:1}" != '#' ]] || continue
  installed="$(pacman -Q "$package" | awk '{print $2}')"
  if [[ "$installed" != "$version" ]]; then
    echo "$package: expected $version, installed $installed" >&2
    exit 1
  fi
  printf '%s %s %s\n' "$package" "$version" "$sha256" >> "$PACKAGE_LOG"
done < "$PACKAGE_LOCK"

rm -rf "$DST"
mkdir -p "$DST/share/icons"

# Index UCRT64 DLLs case-insensitively, as the Windows loader does.
declare -A available seen staged
declare -a queue
while IFS= read -r -d '' dll; do
  available["$(basename "${dll,,}")"]="$dll"
done < <(find "$SRC_BIN" -maxdepth 1 -type f -iname '*.dll' -print0)
queue=("$RELEASE_DIR/tidemark.exe" "$RELEASE_DIR/tidemarkd.exe")

system32="$(cygpath "${SYSTEMROOT:-C:\\Windows}")/System32"
: > "$IMPORT_LOG"
while ((${#queue[@]})); do
  binary="${queue[0]}"
  queue=("${queue[@]:1}")
  key="${binary,,}"
  [[ -z "${seen[$key]:-}" ]] || continue
  seen["$key"]=1

  mapfile -t imports < <(objdump -p "$binary" | sed -n 's/^[[:space:]]*DLL Name:[[:space:]]*//p')
  printf '%s:' "$binary" >> "$IMPORT_LOG"
  printf ' %s' "${imports[@]}" >> "$IMPORT_LOG"
  printf '\n' >> "$IMPORT_LOG"
  for name in "${imports[@]}"; do
    lower="${name,,}"
    if [[ -n "${available[$lower]:-}" ]]; then
      source="${available[$lower]}"
      if [[ -z "${staged[$lower]:-}" ]]; then
        owner="$(pacman -Qqo "$source")"
        owner_version="$(pacman -Q "$owner" | awk '{print $2}')"
        grep -Fqx "$owner $owner_version $(awk -v p="$owner" '$1 == p {print $3}' "$PACKAGE_LOCK")" "$PACKAGE_LOCK" || {
          echo "import $name belongs to unlocked package $owner $owner_version" >&2
          exit 1
        }
        staged["$lower"]="$source"
        queue+=("$source")
      fi
    elif [[ "$lower" == api-ms-win-* || "$lower" == ext-ms-win-* || -f "$system32/$name" ]]; then
      : # Windows system component; never redistribute it.
    else
      echo "cannot resolve non-system import $name required by $binary" >&2
      exit 1
    fi
  done
done

for lower in "${!staged[@]}"; do cp "${staged[$lower]}" "$DST/"; done
printf '%s\n' "${!staged[@]}" | LC_ALL=C sort > "$DLL_LOG"

# Tidemark's own artwork: the provider marks go to share/icons/hicolor, where the client
# looks beside its executable (crates/tidemark/src/marks.rs), and the Start/taskbar icon
# lands beside share/ as tidemark.ico. Rubik is compiled into the client.
APP_ICONS="$SCRIPT_DIR/../../../data/icons"
cp -r "$APP_ICONS/hicolor" "$DST/share/icons/"
cp "$APP_ICONS/tidemark.ico" "$DST/share/tidemark.ico"

echo "staged ${#staged[@]} transitive runtime DLLs from $PREFIX"
echo "runtime package lock: $PACKAGE_LOG"
echo "runtime DLL list: $DLL_LOG"
du -sh "$DST"
