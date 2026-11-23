#!/bin/sh
# Package a built `omt` into the release tarball: bin/omt, ext-host/, LICENSE,
# NOTICE.md at the archive root, so unpacking it anywhere yields a runnable
# prefix (the binary finds `<prefix>/ext-host/host.ts`) and `install.sh` can
# lay the same tree out under `~/.local/share/omt/versions/<version>/`.
#   scripts/package-release.sh BIN OUT.tar.gz [BIN_NAME] [ROOT]
# BIN_NAME defaults to `omt` (Windows assets ship `omt.exe`); ROOT defaults to
# the repository this script lives in.
set -eu
bin=$1
out=$2
bin_name=${3:-omt}
root=${4:-$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)}

for f in "$bin" "$root/LICENSE" "$root/NOTICE.md" "$root/ext-host/host.ts"; do
    if [ ! -e "$f" ]; then
        echo "package-release: missing $f" >&2
        exit 1
    fi
done

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT INT TERM
mkdir -p "$stage/bin"
cp "$bin" "$stage/bin/$bin_name"
chmod +x "$stage/bin/$bin_name"
cp -R "$root/ext-host" "$stage/ext-host"
cp "$root/LICENSE" "$root/NOTICE.md" "$stage/"

mkdir -p "$(dirname -- "$out")"
tar -C "$stage" -czf "$out" bin ext-host LICENSE NOTICE.md
