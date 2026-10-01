#!/usr/bin/env bash
# Builds and runs rashomon-kernel in one step.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$script_dir/.."

export CEF_PATH="${CEF_PATH:-$HOME/.local/share/cef}"

if [[ "$(uname -s)" == "Darwin" ]]; then
    # `cargo run -p rashomon-kernel` alone doesn't work on macOS: CEF's
    # LibraryLoader hardcodes a path lookup for
    # `<exe>/../Frameworks/Chromium Embedded Framework.framework` and
    # panics if that structure doesn't exist, so the binary can only run
    # from inside a bundle `bundle-cef-app` produced — see the module
    # doc comment in src/main.rs and crates/cef-spike for how this was
    # found.
    if ! command -v bundle-cef-app >/dev/null 2>&1; then
        echo "error: bundle-cef-app not found on PATH." >&2
        echo "Install it with: cargo install cef --version 154.0.0 --features build-util" >&2
        exit 1
    fi

    bundle-cef-app rashomon-kernel -o target/bundle
    exec target/bundle/rashomon-kernel.app/Contents/MacOS/rashomon-kernel "$@"
fi

# On Linux, CEF is linked directly and cef-dll-sys's build.rs copies
# its runtime files next to the built binary — no app-bundle structure
# needed. The only Linux-specific wrinkle is that CEF's downloaded
# binaries assume FHS paths, which NixOS doesn't have; patch-cef-linux.sh
# is a no-op when that doesn't apply (e.g. non-NixOS distros).
cargo build -p rashomon-kernel
"$script_dir/patch-cef-linux.sh"
exec cargo run -p rashomon-kernel --bin rashomon-kernel -- "$@"
