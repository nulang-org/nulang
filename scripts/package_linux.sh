#!/usr/bin/env bash
# Package the exact release-tested Linux ELF as native .deb and .rpm files.
# Usage: package_linux.sh <binary> <X.Y.Z> <x86_64|aarch64> <output-dir>
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: $0 <binary> <version> <x86_64|aarch64> <output-dir>" >&2
  exit 2
fi

binary="$(realpath "$1")"
version="${2#v}"
arch="$3"
output="$4"

if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
  echo "Nulang binary not found or not executable: $binary" >&2
  exit 2
fi
# Fail closed on unsupported pre-release identifiers rather than allowing
# untrusted release tags into Debian control fields or RPM spec files.
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "unsupported package version: $version (expected X.Y.Z)" >&2
  exit 2
fi

case "$arch" in
  x86_64)
    deb_arch=amd64
    elf_machine='Advanced Micro Devices X86-64'
    ;;
  aarch64)
    deb_arch=arm64
    elf_machine=AArch64
    ;;
  *) echo "unsupported Linux architecture: $arch" >&2; exit 2 ;;
esac

for tool in readelf dpkg-deb rpmbuild sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "missing required packaging tool: $tool" >&2
    exit 2
  }
done

machine="$(readelf -h "$binary" | sed -nE 's/^[[:space:]]*Machine:[[:space:]]*//p')"
if [ "$machine" != "$elf_machine" ]; then
  echo "binary ELF architecture does not match requested $arch: $machine" >&2
  exit 1
fi

mkdir -p "$output"
output="$(realpath "$output")"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Both package managers install the conventional executable path, rather than
# the platform-qualified filename kept inside the portable release tarballs.
mkdir -p "$tmp/deb/DEBIAN" "$tmp/deb/usr/bin"
install -m 0755 "$binary" "$tmp/deb/usr/bin/nulang"
cat > "$tmp/deb/DEBIAN/control" <<CONTROL
Package: nulang
Version: $version
Section: devel
Priority: optional
Architecture: $deb_arch
Maintainer: David Porkka <37228537+dporkka@users.noreply.github.com>
Depends: libc6 (>= 2.35), libssl3 | libssl3t64
Homepage: https://nulang.org
Description: Nulang programming language compiler and runtime
 Typed concurrent, durable, and distributed programming language CLI.
 The prebuilt CLI excludes Python interoperability.
CONTROL

deb="nulang_${version}_${deb_arch}.deb"
dpkg-deb --root-owner-group --build "$tmp/deb" "$output/$deb"

# No source rebuild: RPM gets the same stripped binary as the Debian archive.
# --target plus explicit BuildArch keeps the RPM architecture deterministic.
mkdir -p "$tmp/rpm"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
install -m 0755 "$binary" "$tmp/rpm/SOURCES/nulang"
cat > "$tmp/rpm/SPECS/nulang.spec" <<SPEC
Name: nulang
Version: $version
Release: 1
Summary: Nulang programming language compiler and runtime
License: Apache-2.0
URL: https://nulang.org
BuildArch: $arch

# This package is an exact copy of the binary already stripped and verified
# by release CI. Default RPM buildroot policy (brp-strip and friends) can
# rewrite ELF bytes, breaking our Debian/RPM/archive payload identity check.
# Do not post-process the binary: our CI inspects and executes the payload.
%global __os_install_post %{nil}

%description
Typed concurrent, durable, and distributed programming language CLI.
The prebuilt CLI excludes Python interoperability.

%prep
%build
%install
install -D -m 0755 %{_sourcedir}/nulang %{buildroot}/usr/bin/nulang

%files
%defattr(-,root,root,-)
/usr/bin/nulang
SPEC

rpmbuild --define "_topdir $tmp/rpm" --target "$arch" -bb "$tmp/rpm/SPECS/nulang.spec"
rpm="$tmp/rpm/RPMS/$arch/nulang-${version}-1.${arch}.rpm"
if [ ! -s "$rpm" ]; then
  echo "expected RPM was not created: $rpm" >&2
  exit 1
fi
mv "$rpm" "$output/"

(
  cd "$output"
  sha256sum "$deb" > "$deb.sha256"
  sha256sum "$(basename "$rpm")" > "$(basename "$rpm").sha256"
)
echo "Packaged Linux $arch: $deb and $(basename "$rpm")"
