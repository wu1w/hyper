#!/usr/bin/env bash
# Restore a snapshot from scripts/hyper-self-snapshot.sh
set -euo pipefail

HOME_HYPER="${HYPER_HOME:-$HOME/.grok-hyper}"
ID="${1:-}"
if [[ -z "$ID" ]]; then
  echo "usage: $0 <snapshot-id>" >&2
  echo "available:" >&2
  ls -1 "$HOME_HYPER/snapshots" 2>/dev/null || true
  exit 2
fi

OUT="$HOME_HYPER/snapshots/$ID"
if [[ ! -f "$OUT/SNAPSHOT.txt" ]]; then
  echo "missing $OUT/SNAPSHOT.txt" >&2
  exit 1
fi

# parse key=value
head_commit=""
stash=""
git_root=""
while IFS='=' read -r k v; do
  case "$k" in
    head) head_commit="$v" ;;
    stash) stash="$v" ;;
    git_root) git_root="$v" ;;
  esac
done < "$OUT/SNAPSHOT.txt"

if [[ -f "$OUT/config.toml" ]]; then
  mkdir -p "$HOME_HYPER"
  cp "$OUT/config.toml" "$HOME_HYPER/config.toml"
  echo "restored config.toml"
fi

# official sidecars
if [[ -d "$HOME_HYPER/sessions" ]]; then
  find "$OUT" -maxdepth 1 -name '*.official.json' -exec cp {} "$HOME_HYPER/sessions/" \; 2>/dev/null || true
fi

if [[ -n "$stash" && -n "$git_root" && -d "$git_root/.git" ]]; then
  echo "working tree blob $stash (not applied)"
  echo "to restore files: git -C \"$git_root\" stash apply $stash"
  echo "to hard reset to snapshot HEAD: git -C \"$git_root\" reset --hard ${head_commit:-HEAD}"
elif [[ -n "$head_commit" && -n "$git_root" && -d "$git_root/.git" ]]; then
  echo "snapshot HEAD $head_commit"
  echo "to hard reset: git -C \"$git_root\" reset --hard $head_commit"
fi

echo "restart hyper web after a config restore"
