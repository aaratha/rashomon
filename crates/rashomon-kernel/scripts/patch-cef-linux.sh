#!/usr/bin/env bash
# cef-dll-sys's build.rs downloads CEF's generic prebuilt Linux
# binaries and lays them out correctly under $CEF_PATH, but those
# binaries assume FHS paths (e.g. /lib64/ld-linux-x86-64.so.2 and
# system libdirs for GTK/NSS/etc.), which don't exist on NixOS. This
# patches the ones that are actually present with a NixOS-correct
# rpath/interpreter so they load. Safe to rerun.
#
# nixpkgs' own `cef-binary` package (and this crate's `NIX_CEF_BINARY`
# support that builds on it) assumes CEF's older file layout and
# unconditionally patches `libEGL.so`/`libGLESv2.so`; CEF 154's Linux
# "minimal" archive no longer ships those, so that path fails. Patching
# directly here only touches files that exist.
set -euo pipefail

CEF_PATH="${CEF_PATH:-$HOME/.local/share/cef}"

if ! command -v nix-instantiate >/dev/null 2>&1; then
    # Not NixOS/Nix — the downloaded binaries assume a standard FHS
    # layout, which should already work unmodified.
    exit 0
fi

if ! command -v patchelf >/dev/null 2>&1; then
    exec nix-shell -p patchelf --run "CEF_PATH='$CEF_PATH' bash '$0'"
fi

nix_libs='
with (import <nixpkgs> {});
lib.makeLibraryPath [
  glib nss nspr atk at-spi2-atk libdrm expat libxkbcommon
  libgbm gtk3 pango cairo alsa-lib dbus at-spi2-core cups
  libGL udev systemdLibs
  libxcb libx11 libxcomposite libxdamage libxext libxfixes libxrandr libxshmfence
]'
nix_gl_libs='with (import <nixpkgs> {}); lib.makeLibraryPath [ stdenv.cc.cc ]'
nix_interp='with (import <nixpkgs> {}); "${stdenv.cc.bintools.dynamicLinker}"'

rpath=$(nix-instantiate --eval --raw -E "$nix_libs")
gl_rpath=$(nix-instantiate --eval --raw -E "$nix_gl_libs")
interp=$(nix-instantiate --eval --raw -E "$nix_interp")

patch_dir() {
    local dir="$1"
    local stamp="$dir/.rashomon-cef-patched"
    local stamp_content="$rpath|$gl_rpath|$interp"
    if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$stamp_content" ]; then
        # Already patched for this rpath/interpreter — skip so we don't
        # bump mtimes and trigger cargo's rerun-if-changed on every run.
        return 0
    fi
    echo "patching CEF binaries in $dir"
    [ -f "$dir/chrome-sandbox" ] &&
        patchelf --set-rpath "$rpath" --set-interpreter "$interp" "$dir/chrome-sandbox"
    [ -f "$dir/libcef.so" ] &&
        patchelf --add-needed libudev.so --set-rpath "$rpath" "$dir/libcef.so"
    [ -f "$dir/libEGL.so" ] &&
        patchelf --set-rpath "$gl_rpath" "$dir/libEGL.so"
    [ -f "$dir/libGLESv2.so" ] &&
        patchelf --add-needed libGL.so.1 --set-rpath "$gl_rpath" "$dir/libGLESv2.so"
    [ -f "$dir/libvk_swiftshader.so" ] &&
        patchelf --set-rpath "$gl_rpath" "$dir/libvk_swiftshader.so"
    [ -f "$dir/libvulkan.so.1" ] &&
        patchelf --set-rpath "$gl_rpath" "$dir/libvulkan.so.1"
    echo "$stamp_content" >"$stamp"
    return 0
}

shopt -s nullglob
for dir in "$CEF_PATH"/*/cef_linux_x86_64 "$CEF_PATH"/*/cef_linux_aarch64; do
    patch_dir "$dir"
done
