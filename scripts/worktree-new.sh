#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 <name> [base-ref]" >&2
  echo "example: $0 jit-cache main" >&2
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage
  exit 2
fi

name="$1"
base_ref="${2:-main}"
branch="agent/$name"

if ! git check-ref-format --branch "$branch" >/dev/null 2>&1; then
  echo "error: '$name' does not produce a valid Git branch name" >&2
  exit 2
fi

repo_root="$(git rev-parse --show-toplevel)"
repo_name="$(basename "$repo_root")"
slug="${name//\//-}"
worktree_root="${NULANG_WORKTREE_ROOT:-$(dirname "$repo_root")/.worktrees/$repo_name}"
worktree_path="$worktree_root/$slug"

if [[ -e "$worktree_path" ]]; then
  echo "error: worktree path already exists: $worktree_path" >&2
  exit 1
fi

if git -C "$repo_root" show-ref --verify --quiet "refs/heads/$branch"; then
  echo "error: local branch already exists: $branch" >&2
  exit 1
fi

mkdir -p "$worktree_root"

echo "==> updating origin/$base_ref"
git -C "$repo_root" fetch origin "$base_ref"

echo "==> creating $branch at $worktree_path"
git -C "$repo_root" worktree add -b "$branch" "$worktree_path" "origin/$base_ref"

echo "==> bootstrapping tools and dependencies"
(
  cd "$worktree_path"
  mise run setup
)

cat <<EOF

Ready:
  path:   $worktree_path
  branch: $branch

Open with:
  zed "$worktree_path"
EOF
