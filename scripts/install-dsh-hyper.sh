#!/usr/bin/env bash
# Optional: build hyper if needed, then install the dsh profile + plugin.
# Product shell is `hyper web`. This path is for people who already run dsh.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if ! command -v hyper >/dev/null 2>&1; then
  echo "building hyper…"
  cargo build -p hyper-cli --release
  export PATH="$root/target/release:$PATH"
  export HYPER_BIN="$root/target/release/hyper"
fi

export HYPER_DSH_PLUGIN="${HYPER_DSH_PLUGIN:-$root/plugins/dsh-plugin-hyper}"
exec hyper dsh-install "$@"
