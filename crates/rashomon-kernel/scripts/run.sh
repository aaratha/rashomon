#!/usr/bin/env bash
# Builds, bundles (via bundle-cef-app), and runs rashomon-kernel in one
# step. `cargo run -p rashomon-kernel` alone doesn't work: CEF's
# LibraryLoader hardcodes a path lookup for
# `<exe>/../Frameworks/Chromium Embedded Framework.framework` and
# panics if that structure doesn't exist, so the binary can only run
# from inside a bundle `bundle-cef-app` produced — see the module doc
# comment in src/main.rs and crates/cef-spike for how this was found.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

export CEF_PATH="${CEF_PATH:-$HOME/.local/share/cef}"

if ! command -v bundle-cef-app >/dev/null 2>&1; then
    echo "error: bundle-cef-app not found on PATH." >&2
    echo "Install it with: cargo install cef --version 154.0.0 --features build-util" >&2
    exit 1
fi

bundle-cef-app rashomon-kernel -o target/bundle
exec target/bundle/rashomon-kernel.app/Contents/MacOS/rashomon-kernel "$@"
