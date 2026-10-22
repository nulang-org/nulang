#!/usr/bin/env bash
# Refresh only the signed Ubuntu repositories required for Python headers.
# GitHub-hosted images can include unrelated third-party APT feeds; an
# outage/403 in those feeds must not make Nulang's Rust test gates unavailable.
set -euo pipefail

ubuntu_sources=/etc/apt/sources.list.d/ubuntu.sources
if [[ ! -f "${ubuntu_sources}" ]]; then
  echo "Expected Ubuntu 24.04 package sources at ${ubuntu_sources}" >&2
  exit 1
fi

# Disables unrelated /etc/apt/sources.list.d entries without weakening APT
# signature verification, changing third-party configuration, or using --allow-*.
sudo apt-get update \
  -o Dir::Etc::sourcelist="sources.list.d/ubuntu.sources" \
  -o Dir::Etc::sourceparts="-"
sudo apt-get install -y --no-install-recommends python3-dev
