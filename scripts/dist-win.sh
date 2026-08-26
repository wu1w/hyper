#!/bin/sh
# Windows x64 zip: console + hyper.exe sidecar + Electron.
# Cross-compile needs cargo-xwin (or mingw-w64) and Homebrew llvm/lld on macOS.
set -eu
ROOT="$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"
cd "$ROOT/web/desktop"
npm install
npm run dist:win
echo
echo "artifacts:"
ls -lh release/*-win.zip 2>/dev/null || ls -lh release/
