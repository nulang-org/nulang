#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fixture="$tmp/releases"
payload="$tmp/payload"
install_dir="$tmp/bin"
mkdir -p "$fixture" "$payload"

cat > "$payload/nulang-linux-x86_64" <<'EOF'
#!/bin/sh
echo nulang-installer-fixture
EOF
chmod +x "$payload/nulang-linux-x86_64"

tar czf "$fixture/nulang-linux-x86_64.tar.gz" -C "$payload" nulang-linux-x86_64
(
  cd "$fixture"
  sha256sum nulang-linux-x86_64.tar.gz > nulang-linux-x86_64.tar.gz.sha256
)

NULANG_INSTALL_OS=linux \
NULANG_INSTALL_ARCH=x86_64 \
NULANG_INSTALL_DIR="$install_dir" \
NULANG_INSTALL_BASE_URL="file://$fixture" \
NULANG_INSTALL_ALLOW_INSECURE=1 \
sh "$repo_root/install.sh"

test -x "$install_dir/nulang"
test "$("$install_dir/nulang")" = "nulang-installer-fixture"

if NULANG_INSTALL_OS=plan9 \
   NULANG_INSTALL_ARCH=x86_64 \
   NULANG_INSTALL_DIR="$tmp/unsupported" \
   NULANG_INSTALL_BASE_URL="file://$fixture" \
   NULANG_INSTALL_ALLOW_INSECURE=1 \
   sh "$repo_root/install.sh" >"$tmp/unsupported.log" 2>&1; then
  echo "installer unexpectedly accepted an unsupported OS" >&2
  exit 1
fi
grep -q "unsupported operating system" "$tmp/unsupported.log"

bad="$tmp/bad"
mkdir -p "$bad"
cp "$fixture/nulang-linux-x86_64.tar.gz" "$bad/"
printf '%064d  nulang-linux-x86_64.tar.gz\n' 0 > "$bad/nulang-linux-x86_64.tar.gz.sha256"

if NULANG_INSTALL_OS=linux \
   NULANG_INSTALL_ARCH=x86_64 \
   NULANG_INSTALL_DIR="$tmp/bad-bin" \
   NULANG_INSTALL_BASE_URL="file://$bad" \
   NULANG_INSTALL_ALLOW_INSECURE=1 \
   sh "$repo_root/install.sh" >"$tmp/checksum.log" 2>&1; then
  echo "installer unexpectedly accepted a bad checksum" >&2
  exit 1
fi
grep -Eq "FAILED|checksum" "$tmp/checksum.log"

echo "installer fixture tests passed"
