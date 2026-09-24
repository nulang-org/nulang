#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

errors=0
warnings=0

ok()   { printf 'ok: %s\n' "$*"; }
warn() { printf 'warn: %s\n' "$*" >&2; warnings=$((warnings + 1)); }
fail() { printf 'error: %s\n' "$*" >&2; errors=$((errors + 1)); }

need() {
  local cmd="$1"
  if command -v "$cmd" >/dev/null 2>&1; then
    ok "$cmd -> $(command -v "$cmd")"
  else
    fail "missing required command: $cmd"
  fi
}

printf 'Nulang development environment doctor\n\n'

for cmd in git rustup rustc cargo python3 cc pkg-config mbx; do
  need "$cmd"
done

if command -v mise >/dev/null 2>&1; then
  if mise doctor >/dev/null 2>&1; then
    ok "mise doctor"
  else
    fail "mise doctor reported a problem (run: mise doctor)"
  fi
else
  fail "missing required command: mise"
fi

if command -v rustc >/dev/null 2>&1; then
  rust_version="$(rustc --version | awk '{print $2}')"
  expected="$(awk -F'"' '/^channel/ {print $2; exit}' rust-toolchain.toml)"
  if [[ -n "$expected" && "$rust_version" == "$expected" ]]; then
    ok "Rust $rust_version matches rust-toolchain.toml"
  else
    fail "Rust version mismatch: have $rust_version, expected ${expected:-unknown}"
  fi
fi

if command -v cargo >/dev/null 2>&1; then
  if cargo metadata --locked --no-deps --format-version 1 >/dev/null 2>&1; then
    ok "Cargo.lock resolves with --locked"
  else
    fail "Cargo metadata failed with --locked"
  fi
fi

if command -v findmnt >/dev/null 2>&1; then
  fs_type="$(findmnt -no FSTYPE --target "$ROOT" 2>/dev/null || true)"
  case "$fs_type" in
    nfs|nfs4|cifs|smb3|fuse.sshfs)
      fail "repository is on $fs_type; use local storage for Cargo/mbx build data"
      ;;
    "")
      warn "could not determine repository filesystem type"
      ;;
    *)
      ok "repository filesystem: $fs_type"
      ;;
  esac
else
  warn "findmnt not available; filesystem locality check skipped"
fi

available_kb="$(df -Pk "$ROOT" | awk 'NR==2 {print $4}')"
if [[ "$available_kb" =~ ^[0-9]+$ ]]; then
  available_gb=$((available_kb / 1024 / 1024))
  if (( available_gb < 10 )); then
    fail "low free disk space: ${available_gb} GiB (10 GiB minimum)"
  elif (( available_gb < 25 )); then
    warn "free disk space is only ${available_gb} GiB; full matrices/benchmarks may need more"
  else
    ok "free disk space: ${available_gb} GiB"
  fi
fi

if [[ -n "${CI:-}" ]]; then
  ok "CI environment detected"
elif [[ -n "$(git status --porcelain 2>/dev/null || true)" ]]; then
  warn "working tree has uncommitted changes"
else
  ok "working tree is clean"
fi

printf '\n'
if (( errors > 0 )); then
  printf 'doctor failed: %d error(s), %d warning(s)\n' "$errors" "$warnings" >&2
  exit 1
fi

printf 'doctor passed with %d warning(s)\n' "$warnings"
