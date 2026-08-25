#!/bin/sh
# Mac .app / .dmg: console + hyper sidecar + Electron. Same flow as q-harness
# `web/desktop` `npm run dist:mac`, plus a console rebuild so the bundle is current.
set -eu
ROOT="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
cd "$ROOT/web/desktop"
npm install
npm run dist:mac
echo
echo "artifacts:"
ls -lh release/*.dmg release/*-arm64-mac.zip 2>/dev/null || ls -lh release/
