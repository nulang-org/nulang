#!/bin/sh
set -eu

die() {
    printf 'nulang installer: %s\n' "$*" >&2
    exit 1
}

repo="${NULANG_INSTALL_REPO:-nulang-org/nulang}"
install_dir="${NULANG_INSTALL_DIR:-${HOME:-}/.local/bin}"
version="${NULANG_INSTALL_VERSION:-}"

[ -n "$install_dir" ] || die "HOME is not set; set NULANG_INSTALL_DIR explicitly"

raw_os="${NULANG_INSTALL_OS:-$(uname -s 2>/dev/null || true)}"
raw_arch="${NULANG_INSTALL_ARCH:-$(uname -m 2>/dev/null || true)}"

case "$raw_os" in
    Linux|linux) os="linux" ;;
    Darwin|darwin|macOS|macos) os="macos" ;;
    *) die "unsupported operating system '$raw_os'; use a release archive from https://github.com/$repo/releases" ;;
esac

case "$raw_arch" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) die "unsupported architecture '$raw_arch'; use a release archive from https://github.com/$repo/releases" ;;
esac

case "$os/$arch" in
    linux/x86_64|linux/aarch64|macos/aarch64) ;;
    *) die "no release artifact for $os/$arch; use https://github.com/$repo/releases or build from source" ;;
esac

artifact="nulang-$os-$arch"
archive="$artifact.tar.gz"
checksum="$archive.sha256"

if [ -n "${NULANG_INSTALL_BASE_URL:-}" ]; then
    base_url="$NULANG_INSTALL_BASE_URL"
elif [ -n "$version" ]; then
    base_url="https://github.com/$repo/releases/download/$version"
else
    base_url="https://github.com/$repo/releases/latest/download"
fi

case "$base_url" in
    https://*) ;;
    *)
        [ "${NULANG_INSTALL_ALLOW_INSECURE:-0}" = "1" ] ||
            die "refusing non-HTTPS download URL; set NULANG_INSTALL_ALLOW_INSECURE=1 only for controlled testing"
        ;;
esac

if command -v curl >/dev/null 2>&1; then
    download() {
        curl --fail --silent --show-error --location "$1" --output "$2"
    }
elif command -v wget >/dev/null 2>&1; then
    download() {
        wget -qO "$2" "$1"
    }
else
    die "curl or wget is required"
fi

tmp="${TMPDIR:-/tmp}/nulang-install.$$"
umask 077
mkdir -p "$tmp"
cleanup() {
    rm -rf "$tmp"
}
trap cleanup EXIT HUP INT TERM

printf 'Downloading %s...\n' "$archive"
download "$base_url/$archive" "$tmp/$archive"
download "$base_url/$checksum" "$tmp/$checksum"

printf 'Verifying SHA-256 checksum...\n'
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$tmp" && sha256sum -c "$checksum")
elif command -v shasum >/dev/null 2>&1; then
    (cd "$tmp" && shasum -a 256 -c "$checksum")
else
    die "sha256sum or shasum is required to verify the release"
fi

tar xzf "$tmp/$archive" -C "$tmp"
[ -f "$tmp/$artifact" ] || die "archive did not contain expected executable '$artifact'"

mkdir -p "$install_dir"
cp "$tmp/$artifact" "$install_dir/nulang"
chmod 0755 "$install_dir/nulang"

printf 'Installed Nulang to %s/nulang\n' "$install_dir"
case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *)
        printf 'Add %s to PATH, for example:\n  export PATH="%s:$PATH"\n' "$install_dir" "$install_dir"
        ;;
esac
printf 'Run: nulang --version\n'
