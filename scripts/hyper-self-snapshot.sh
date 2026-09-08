#!/usr/bin/env bash
# Snapshot Hyper config + git HEAD so a self-edit can roll back.
set -euo pipefail

HOME_HYPER="${HYPER_HOME:-$HOME/.grok-hyper}"
ID="$(date +%Y%m%dT%H%M%S)-$$"
OUT="$HOME_HYPER/snapshots/$ID"
mkdir -p "$OUT"

if [[ -f "$HOME_HYPER/config.toml" ]]; then
  cp "$HOME_HYPER/config.toml" "$OUT/config.toml"
fi
# official compact sidecars (best-effort)
if [[ -d "$HOME_HYPER/sessions" ]]; then
  find "$HOME_HYPER/sessions" -maxdepth 1 -name '*.official.json' -exec cp {} "$OUT/" \; 2>/dev/null || true
fi

GIT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
HEAD=""
STASH=""
if [[ -n "$GIT_ROOT" ]]; then
  HEAD="$(git -C "$GIT_ROOT" rev-parse HEAD)"
  # dangling commit of the working tree; does not change HEAD or the index
  STASH="$(git -C "$GIT_ROOT" stash create "hyper-self $ID" || true)"
  git -C "$GIT_ROOT" rev-parse --abbrev-ref HEAD > "$OUT/branch.txt" || true
fi

{
  echo "id=$ID"
  echo "created=$(date -Iseconds)"
  echo "cwd=$(pwd)"
  echo "git_root=${GIT_ROOT:-}"
  echo "head=${HEAD:-}"
  echo "stash=${STASH:-}"
  echo "config=$OUT/config.toml"
} > "$OUT/SNAPSHOT.txt"

echo "snapshot $ID"
echo "dir $OUT"
[[ -n "$HEAD" ]] && echo "head $HEAD"
[[ -n "$STASH" ]] && echo "stash $STASH"
echo "rollback: scripts/hyper-self-rollback.sh $ID"
