#!/usr/bin/env bash
# Snapshot the hyper checkout (and a copy of live config) before the
# in-repo agent edits itself. Not shadow-git; just annotated tags + a bak.
set -euo pipefail
root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "self-snap: run inside the hyper git checkout" >&2
  exit 1
}
cd "$root"
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "self-snap: warning: working tree is dirty; tag still points at HEAD" >&2
  git status -sb
fi
ts=$(date -u +%Y%m%dT%H%M%SZ)
tag="hyper-snap/${ts}"
git tag -a "$tag" -m "self-snap $ts"
echo "tagged $tag -> $(git rev-parse --short HEAD)"
cfg="${HOME}/.grok-hyper/config.toml"
if [[ -f "$cfg" ]]; then
  dest="${HOME}/.grok-hyper/config.toml.bak.${ts}"
  cp "$cfg" "$dest"
  echo "copied $cfg -> $dest"
fi
echo "rollback source:  git checkout $tag -- <file>"
echo "rollback tree:    git reset --hard $tag"
echo "rollback session: /undo   (does not revert files)"
