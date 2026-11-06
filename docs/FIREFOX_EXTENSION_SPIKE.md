# Rashomon Firefox-extension Host — Stage 0 spike: validate native-messaging transport

Rashomon is a Rust-based "desktop environment built on a persistent graph"
(see `rashomon.org`/`README.md`). The kernel (`crates/rashomon-kernel`) is a
native process owning a WASM component runtime, graph storage, and
primitives like `rashomon:browser` and `rashomon:process`. It's being
pivoted from a CEF-based custom shell Host to a **Firefox WebExtension
Host**, forking [Sidebery](https://github.com/mbnuqw/sidebery)
(MIT-licensed tree-tab sidebar extension) for the UI. Work happens on the
`firefox-extension` branch (off clean `main`); the old CEF/Chrome-extension-
compat approach is preserved on the `native-client` branch for reference —
don't build on `crates/cef-extension-spike`/`cef-winit-spike`, they're
superseded.

**Goal of this spike**: prove the native-messaging transport end-to-end —
Firefox launching the kernel as a native app and exchanging messages —
before any real graph or primitive work begins. Nothing here should touch
`rashomon:graph`, `rashomon:browser`, or `rashomon:ui`.

## Tasks

1. Fork Sidebery into the repo (e.g. a new top-level `host-firefox/sidebery/`).
   Keep its own build system (pnpm/whatever it uses) intact and buildable;
   don't modify its tab-tree logic yet.
2. Add a native-messaging entry point to `crates/rashomon-kernel` (new bin
   target or a CLI flag on the existing binary) that implements Firefox's
   native messaging stdio protocol: 4-byte little-endian length prefix +
   UTF-8 JSON, both directions. On receiving any message, log it and send
   back an ack.
3. Write and register the native messaging host manifest (`type: "stdio"`,
   `allowed_extensions` listing the dev extension's temporary ID) at the
   OS-appropriate path (macOS:
   `~/Library/Application Support/Mozilla/NativeMessagingHosts/<app-id>.json`),
   pointing at the compiled binary's absolute path.
4. In the forked Sidebery's background script, call
   `browser.runtime.connectNative("<app-id>")` and relay Sidebery's existing
   `tabs.onCreated`/`onUpdated`/`onRemoved`/`onMoved` listeners' raw event
   payloads over the port. Don't change what Sidebery does with those
   events internally — just tap them.
5. Load the fork as a temporary add-on (`about:debugging`) and confirm
   opening/closing/dragging tabs produces corresponding log lines on the
   kernel side.

## Definition of done

A tab action in Firefox produces a logged, correctly-shaped message on the
native-messaging host within ~1s, and the host relaunches correctly if
Firefox restarts the connection.

## Explicitly out of scope

Any graph writes, any WIT/primitive changes, any `rashomon:ui` rendering,
any change to Sidebery's own tree/data model. That's Stage 1+.
