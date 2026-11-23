#!/bin/sh
# Install omt (oh my tau).
#   curl -fsSL https://raw.githubusercontent.com/jc01rho/oh-my-tau/main/install.sh | sh
# Picks the release tarball for Linux x86_64/aarch64 and macOS x86_64/arm64,
# verifies its sha256 against SHA256SUMS.txt, and unpacks it under
# ${OMT_HOME:-$HOME/.local/share/omt}/versions/<version>/ with bin/omt,
# ext-host/ (the Bun extension host) and the license files.
# ${OMT_INSTALL_DIR:-$HOME/.local/bin}/omt points at the newest install.
# The extension host needs bun; a missing bun fails the install.
set -eu

REPO="${OMT_REPO:-jc01rho/oh-my-tau}"
TAG="${OMT_VERSION:-}"
BASE="https://github.com/${REPO}/releases"
DEST_DIR="${OMT_INSTALL_DIR:-$HOME/.local/bin}"
OMT_HOME="${OMT_HOME:-$HOME/.local/share/omt}"

os=$(uname -s)
arch=$(uname -m)

case "$os" in
    Linux)  platform="linux" ;;
    Darwin) platform="macos" ;;
    *) platform="" ;;
esac
case "$arch" in
    x86_64 | amd64)   machine="x64" ;;
    aarch64 | arm64)  machine="arm64" ;;
    *) machine="" ;;
esac

if [ -z "$platform" ] || [ -z "$machine" ]; then
    echo "omt: no prebuilt binary for ${os} ${arch} (supported: linux/macos x86_64 + aarch64/arm64)." >&2
    echo "Build from source: git clone https://github.com/${REPO} && cd oh-my-tau && cargo build --release -p omp-cli" >&2
    echo "The binary is target/release/omt." >&2
    exit 1
fi

if ! command -v bun >/dev/null 2>&1; then
    echo "omt: bun is required for the extension host (https://bun.sh)." >&2
    exit 1
fi
command -v curl >/dev/null 2>&1 || { echo "omt: curl is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "omt: tar is required" >&2; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$@"; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$@"; }
else
    echo "omt: sha256sum or shasum is required" >&2
    exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

if [ -z "$TAG" ]; then
    tag_url=$(curl -fsSL -o /dev/null -w '%{url_effective}' "${BASE}/latest")
    TAG=${tag_url##*/}
    if [ -z "$TAG" ] || [ "$TAG" = "latest" ]; then
        echo "omt: cannot resolve the newest release under ${BASE}" >&2
        exit 1
    fi
fi
DOWNLOAD="${BASE}/download/${TAG}"

# Linux: glibc build by default; the fully static musl build where glibc is
# absent (Alpine, distroless) or when OMT_LIBC=musl. The musl build lacks only
# the optional ONNX-based Laya judge.
libc="${OMT_LIBC:-}"
if [ "$platform" = "linux" ] && [ -z "$libc" ]; then
    if ldd --version 2>&1 | grep -qi musl || ! ldd --version >/dev/null 2>&1; then
        libc="musl"
    fi
fi
ASSET="omt-${platform}-${machine}"
if [ "$platform" = "linux" ] && [ "$libc" = "musl" ]; then
    ASSET="${ASSET}-musl"
fi

curl -fsSL -o "$tmp/SHA256SUMS.txt" "${DOWNLOAD}/SHA256SUMS.txt"

fetch() {
    echo "Downloading $1 ${TAG} ..."
    curl -fL --progress-bar -o "$tmp/$1" "${DOWNLOAD}/$1"
    expected=$(awk -v f="$1" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS.txt")
    if [ -z "$expected" ]; then
        echo "omt: $1 not listed in SHA256SUMS.txt" >&2
        exit 1
    fi
    actual=$(sha256 "$tmp/$1" | awk '{ print $1 }')
    if [ "$expected" != "$actual" ]; then
        echo "omt: sha256 mismatch (expected ${expected}, got ${actual}); nothing installed" >&2
        exit 1
    fi
    echo "sha256 ok: ${actual}"
}

install_tarball() {
    name="$1.tar.gz"
    fetch "$name"
    rm -rf "$tmp/unpacked"
    mkdir -p "$tmp/unpacked"
    tar -C "$tmp/unpacked" -xzf "$tmp/$name"
    if [ ! -f "$tmp/unpacked/bin/omt" ]; then
        echo "omt: ${name} has no bin/omt" >&2
        exit 1
    fi
    if [ ! -f "$tmp/unpacked/ext-host/host.ts" ]; then
        echo "omt: ${name} has no ext-host/host.ts" >&2
        exit 1
    fi
    chmod +x "$tmp/unpacked/bin/omt"
}

install_tarball "$ASSET"
# A glibc build that cannot run here (glibc too old): use the static musl build.
if [ "$platform" = "linux" ] && [ "$libc" != "musl" ] && ! "$tmp/unpacked/bin/omt" --version >/dev/null 2>&1; then
    echo "omt: ${ASSET} does not run on this system (glibc too old?); using the static musl build"
    ASSET="${ASSET}-musl"
    install_tarball "$ASSET"
fi

version=$("$tmp/unpacked/bin/omt" --version | sed -n 's|^omt/||p')
if [ -z "$version" ]; then
    echo "omt: the downloaded binary did not report a version" >&2
    exit 1
fi

prefix="${OMT_HOME}/versions/${version}"
mkdir -p "${OMT_HOME}/versions"
rm -rf "${prefix}.tmp" "${prefix}.old"
mv "$tmp/unpacked" "${prefix}.tmp"
# Move the previous install aside first: a failure or interrupt here must not
# leave the launcher pointing at a directory that is already gone.
if [ -e "$prefix" ]; then
    mv "$prefix" "${prefix}.old"
fi
mv "${prefix}.tmp" "$prefix"
rm -rf "${prefix}.old"

mkdir -p "$DEST_DIR"
ln -sf "${prefix}/bin/omt" "$DEST_DIR/omt"
echo "Installed ${DEST_DIR}/omt -> ${prefix}/bin/omt (${version}, ext-host bundled)"

if ! "$DEST_DIR/omt" --version >/dev/null 2>&1; then
    echo "omt: ${DEST_DIR}/omt does not run after installation" >&2
    exit 1
fi
echo "Verified: $("$DEST_DIR/omt" --version)"

case ":${PATH}:" in
    *":${DEST_DIR}:"*) ;;
    *) echo "Add it to PATH:  export PATH=\"${DEST_DIR}:\$PATH\"" ;;
esac
