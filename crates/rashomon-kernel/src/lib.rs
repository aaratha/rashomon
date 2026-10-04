//! Rashomon kernel — proves the Wasmtime Component Model round-trip
//! against several guests: `ping` (minimal render-only demo), `shell`
//! (one-shot process spawn + graph write), `terminal` (a real
//! interactive PTY-backed terminal, parsed guest-side with `vt100` —
//! proven via a one-shot render() call, console-only for now), and
//! `terminal-xterm` (the same PTY, handing the host an `xterm.js` HTML
//! payload — a Facet actually opened as a live View).
//!
//! There's no `rashomon:ui` primitive yet (the design doc leaves its
//! payload format undesigned), so a View's "render surface" is still
//! just the raw HTML string a Facet's `render` returns — but opening a
//! View is no longer one hardcoded Facet filling one hardcoded page.
//! [`Kernel::open_view`] can instantiate any registered Facet against
//! any Entity and mount it as a tab in the one shared switcher window
//! every View and every `rashomon:browser` tab lives in (see
//! [`BrowserSwitcherState`]), all sharing one
//! `Client`/`InputQueryHandler`/`Store` via [`InputBridge`].
//!
//! **Windowing: `winit` owns the one native window, CEF and `wry` are
//! both just children of it.** CEF's own Views framework (`BrowserView`
//! in one shared `Panel`) was the original design and worked, but CEF's
//! `BrowserView` can never be made transparent in windowed mode
//! (upstream issue chromiumembedded/cef#4035, confirmed unfixed) — a
//! hard blocker once the UI grew a translucent sidebar. Validated in
//! `crates/cef-winit-spike` and adopted here: `winit` creates and owns
//! the single top-level window; real browser tabs embed as classic
//! (non-Views) CEF child browsers via `WindowInfo::set_as_child`,
//! staying fully opaque (expected, for a real web page); the sidebar,
//! the urlbar, and every non-browser Facet View tab instead mount as
//! transparent `wry` (WKWebView) child webviews over a real background
//! blur (`mac::apply_background_blur`, a private CoreGraphics Services
//! call — see that function's doc comment), so the glass layer behind
//! them actually shows through. CEF is driven via its documented
//! `external_message_pump` integration mode (see
//! `WinitKernelApp::about_to_wait`) rather than owning its own blocking
//! run loop, since `winit`'s is the one that's actually blocking here.
//!
//! Input flows back via a CEF message-router bridge for real browser
//! tabs, and `wry`'s own IPC transport (behind a `window.cefQuery`
//! polyfill — see `mac::FacetWebView`) for everything else: each tab's
//! page prefixes its request with its own window id, so one shared
//! [`InputQueryHandler`]/[`dispatch_facet_request`] can route a
//! keystroke (or a [`POLL_REQUEST`]) to the right View's `handle-input`
//! (or `poll-output`) rather than there being one handler per tab. More
//! than one tab can mirror the same View this way — see
//! [`Kernel::open_window`] — with output fanned out so no mirroring tab
//! loses it to whichever one happens to poll first. Every
//! primitive is backed by a real host
//! implementation — `rashomon:graph` by a `PersistentGraphStore` (so
//! the graph survives a restart), and `rashomon:process` by a real PTY
//! (`portable-pty`), not plain OS pipes — `resize`/`signal` are genuine
//! now.
//!
//! This is a library, not just `main.rs`, because CEF subprocesses
//! (renderer/GPU/utility) on macOS run through the *separate*
//! `rashomon-kernel-helper` binary `bundle-cef-app` produces, not this
//! crate's own `main.rs` — and the renderer subprocess specifically
//! needs the same `KernelApp`/`RenderProcessHandler` code the browser
//! process uses (to register `window.cefQuery` in the page's JS
//! context), so both binaries link against this shared library instead
//! of duplicating that code.

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Context, Result};
use cef::wrapper::message_router::*;
use cef::*;
use directories::ProjectDirs;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rashomon_graph::GraphStore;
use serde::{Deserialize, Serialize};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
#[cfg(target_os = "macos")]
use winit::platform::macos::WindowAttributesExtMacOS;
use winit::window::{Window, WindowId};

#[cfg(target_os = "macos")]
pub mod mac;

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "facet-world",
    with: {
        "rashomon:process/types.process": SpawnedProcess,
        "rashomon:browser/types.browser-context": BrowserContext,
        "rashomon:browser/types.browser-tab": BrowserTab,
    },
});

/// Host-side state for one component instance. Owns the persistent
/// graph that backs the `rashomon:graph` implementation, plus the WASI
/// Preview 2 context that the cargo-component/wasm32-wasip1 toolchain's
/// guest-side adapter always imports (even though the `ping` guest
/// never calls into it) — see the README for why that's required just
/// to instantiate.
struct KernelState {
    graph: rashomon_graph::PersistentGraphStore,
    wasi_ctx: WasiCtx,
    table: ResourceTable,
    /// The extensions this process loaded at startup (see
    /// [`BrowserExtensionConfig`]) — from the persisted config file,
    /// empty on a fresh install until something's been added.
    extensions: Arc<Vec<ExtensionRuntime>>,
    /// Computed once at startup — extensions found already installed
    /// in another browser, not yet added to `extensions`'s backing
    /// config file. See [`discover_extension_candidates`].
    extension_candidates: Arc<Vec<ExtensionCandidate>>,
    /// `None` until [`WinitKernelApp::resumed`] builds the one native
    /// window every tab gets mounted into — `create_tab` must never be
    /// called before that happens.
    browser_switcher: Arc<Mutex<Option<BrowserSwitcherState>>>,
    /// The id of this process's one session-root `rashomon:thread`
    /// Entity, created once at the very start of
    /// [`run_browser_process`] (see that function's doc comment) —
    /// every Node any Facet creates afterward via `create-node` gets
    /// linked to it automatically (see
    /// `rashomon::graph::store::Host::create_node`'s impl below), and
    /// it's what [`Kernel::open_view`] checks a Node against to decide
    /// whether opening it counts as a *fresh* access to something that
    /// predates this session.
    session_root_id: String,
    /// One `rashomon:page` Entity id per URL any real browser tab has
    /// ever navigated to, across every session — populated once at
    /// startup from whatever's already persisted (see
    /// `run_browser_process`), then kept live-updated by
    /// [`record_page_visit`]. The point: navigating to a URL for the
    /// first time ever creates this Entity; every navigation after
    /// that (this session or a later one) finds it here instead of
    /// creating a duplicate, and gets recorded as an Occurrence of it
    /// instead.
    page_entities: HashMap<String, String>,
    /// A clone of the very `Arc<Mutex<InputBridge>>` that wraps the
    /// `Store` this `KernelState` lives inside — `None` only for the
    /// brief window between that `Arc` being constructed and this
    /// field being set to a clone of it right after (see
    /// `run_browser_process`); every real use happens well after that.
    /// Needed because `rashomon::browser::control::Host::create_tab`
    /// only ever gets `&mut self: KernelState` to work with (the
    /// Host trait's shape is fixed by `wasmtime::component::bindgen!`),
    /// with no way to also receive the surrounding `Arc` as a separate
    /// parameter — but it still needs to hand one to `TabClient` (see
    /// `create_tab`'s body), so a tab's `on_address_change` can safely
    /// re-lock the bridge *later*, from CEF's own independent callback
    /// (not nested inside whatever call is already holding the lock
    /// while `create_tab` itself runs).
    bridge_handle: Option<Arc<Mutex<InputBridge>>>,
}

impl WasiView for KernelState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

impl rashomon::graph::types::Host for KernelState {}

/// Edge type linking a Node to the session-root `rashomon:thread` that
/// was active when it was created — see [`KernelState::session_root_id`].
/// Deliberately distinct from `"occurrence-of"`: that edge means
/// "is an occurrence of this Entity" (and drives cascading deletes —
/// see `rashomon::graph::store::Host::delete_node`'s impl below), a
/// completely different relationship from "was created during this
/// run of the app."
const PART_OF_THREAD_EDGE: &str = "part-of-thread";

impl rashomon::graph::store::Host for KernelState {
    /// Every Node created this way — i.e. by a Facet, through the WIT
    /// boundary, as opposed to the handful `run_browser_process`
    /// creates directly before any Facet runs — gets linked to this
    /// session's root thread automatically, with zero cooperation
    /// needed from the Facet creating it. See
    /// [`KernelState::session_root_id`]'s doc comment.
    fn create_node(
        &mut self,
        node_type: String,
        node_role: rashomon::graph::types::Role,
        properties: Vec<rashomon::graph::types::Property>,
    ) -> rashomon::graph::types::Node {
        let properties = properties.into_iter().map(|p| (p.key, p.value)).collect();
        let node = create_node_in_session(self, &node_type, to_lib_role(node_role), properties);
        to_wit_node(node)
    }

    fn get_node(&mut self, id: String) -> Option<rashomon::graph::types::Node> {
        self.graph.get_node(&id).map(to_wit_node)
    }

    fn create_edge(
        &mut self,
        edge_type: String,
        source: String,
        target: String,
        confidence: f32,
    ) -> rashomon::graph::types::Edge {
        to_wit_edge(self.graph.create_edge(&edge_type, &source, &target, confidence))
    }

    fn query_edges_from(&mut self, node_id: String) -> Vec<rashomon::graph::types::Edge> {
        self.graph
            .query_edges_from(&node_id)
            .into_iter()
            .map(to_wit_edge)
            .collect()
    }

    fn query_edges_to(&mut self, node_id: String) -> Vec<rashomon::graph::types::Edge> {
        self.graph
            .query_edges_to(&node_id)
            .into_iter()
            .map(to_wit_edge)
            .collect()
    }

    fn list_nodes(&mut self) -> Vec<rashomon::graph::types::Node> {
        self.graph.all_nodes().into_iter().map(to_wit_node).collect()
    }

    fn list_edges(&mut self) -> Vec<rashomon::graph::types::Edge> {
        self.graph.all_edges().into_iter().map(to_wit_edge).collect()
    }

    /// `GraphStore::delete_node` (the generic store primitive) has no
    /// idea what an `occurrence-of` Edge means — that cascade is a
    /// `rashomon:graph` domain convention, so it lives here instead:
    /// deleting an `entity` Node first deletes every `occurrence` Node
    /// connected to it via an `occurrence-of` Edge (found the same way
    /// `create_tab`-style Facets create that edge in the first place —
    /// `occurrence-of` points *from* the Occurrence *to* the Entity),
    /// then deletes the Entity itself. Deleting an `occurrence` Node
    /// directly (or any Node whose role can't be determined, e.g. one
    /// that's already gone) just deletes that one Node — no cascade in
    /// that direction.
    fn delete_node(&mut self, id: String) -> bool {
        let is_entity = matches!(
            self.graph.get_node(&id).map(|n| n.role),
            Some(rashomon_graph::Role::Entity)
        );
        if is_entity {
            let occurrence_ids: Vec<String> = self
                .graph
                .query_edges_to(&id)
                .into_iter()
                .filter(|e| e.edge_type == "occurrence-of")
                .map(|e| e.source)
                .collect();
            for occurrence_id in occurrence_ids {
                self.graph.delete_node(&occurrence_id);
            }
        }
        self.graph.delete_node(&id)
    }
}

/// The `rashomon:process` resource backing: a real PTY (`portable-pty`),
/// not plain OS pipes — `resize`/`signal` are genuine now, not stubs.
/// `incoming` is filled by a background thread that blocks on the PTY's
/// reader so `read()` itself never blocks — a live redraw loop calling
/// it every frame must never stall waiting for output that hasn't
/// arrived yet.
pub struct SpawnedProcess {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    incoming: Arc<Mutex<VecDeque<u8>>>,
    /// Joined by `wait()` after the child exits — the reader thread is
    /// guaranteed to observe EOF and stop shortly after that (closing
    /// the child's fds is what generates it), so joining first
    /// guarantees `incoming` holds *all* of the process's output by the
    /// time `wait()` returns to the guest, not just whatever had been
    /// drained so far.
    reader_thread: Option<std::thread::JoinHandle<()>>,
}

impl rashomon::process::types::Host for KernelState {}

impl rashomon::process::types::HostProcess for KernelState {
    fn write(&mut self, self_: Resource<SpawnedProcess>, data: Vec<u8>) -> Result<u32, String> {
        let process = self.table.get_mut(&self_).map_err(|e| e.to_string())?;
        process.writer.write(&data).map(|n| n as u32).map_err(|e| e.to_string())
    }

    fn read(&mut self, self_: Resource<SpawnedProcess>, max_bytes: u32) -> Result<Vec<u8>, String> {
        let process = self.table.get_mut(&self_).map_err(|e| e.to_string())?;
        let mut incoming = process.incoming.lock().expect("incoming buffer lock poisoned");
        let n = (max_bytes as usize).min(incoming.len());
        Ok(incoming.drain(..n).collect())
    }

    fn resize(&mut self, self_: Resource<SpawnedProcess>, cols: u32, rows: u32) -> Result<(), String> {
        let process = self.table.get_mut(&self_).map_err(|e| e.to_string())?;
        process
            .master
            .resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())
    }

    fn signal(&mut self, self_: Resource<SpawnedProcess>, sig: String) -> Result<(), String> {
        let process = self.table.get_mut(&self_).map_err(|e| e.to_string())?;
        // Over a real PTY, INT/QUIT are delivered by writing the
        // control byte the tty driver translates into a signal for the
        // foreground process group — the same way pressing Ctrl-C in a
        // real terminal works. KILL/TERM have no such byte, so those go
        // straight through ChildKiller instead.
        match sig.to_uppercase().as_str() {
            "INT" | "SIGINT" => process.writer.write_all(&[0x03]).map_err(|e| e.to_string()),
            "QUIT" | "SIGQUIT" => process.writer.write_all(&[0x1c]).map_err(|e| e.to_string()),
            "KILL" | "SIGKILL" | "TERM" | "SIGTERM" => process.child.kill().map_err(|e| e.to_string()),
            other => Err(format!("unsupported signal: {other}")),
        }
    }

    fn wait(&mut self, self_: Resource<SpawnedProcess>) -> i32 {
        let process = match self.table.get_mut(&self_) {
            Ok(process) => process,
            Err(_) => return -1,
        };
        let exit_code = match process.child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => -1,
        };
        if let Some(handle) = process.reader_thread.take() {
            let _ = handle.join();
        }
        exit_code
    }

    fn drop(&mut self, self_: Resource<SpawnedProcess>) -> wasmtime::Result<()> {
        if let Ok(mut process) = self.table.delete(self_) {
            let _ = process.child.kill();
        }
        Ok(())
    }
}

impl rashomon::process::spawner::Host for KernelState {
    fn spawn(
        &mut self,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
    ) -> Result<Resource<SpawnedProcess>, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("failed to open pty: {e}"))?;

        let mut builder = CommandBuilder::new(&command);
        builder.args(&args);
        // Overrides whatever `TERM` this GUI process itself inherited
        // (often entirely unset, or `dumb`, when launched outside a
        // real terminal — confirmed empirically: shells/prompts like
        // `starship` explicitly detect and disable themselves under
        // it) with the one every spawned shell is actually running
        // inside here: `terminal-xterm`'s `xterm.js` frontend, which
        // emulates a real `xterm-256color`-class terminal.
        builder.env("TERM", "xterm-256color");
        if let Some(cwd) = &cwd {
            builder.cwd(cwd);
        }

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| format!("failed to spawn `{command}`: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("failed to take pty writer: {e}"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("failed to clone pty reader: {e}"))?;

        let incoming = Arc::new(Mutex::new(VecDeque::new()));
        let incoming_bg = incoming.clone();
        let reader_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => incoming_bg
                        .lock()
                        .expect("incoming buffer lock poisoned")
                        .extend(&buf[..n]),
                    Err(_) => break,
                }
            }
        });

        self.table
            .push(SpawnedProcess {
                master: pair.master,
                reader_thread: Some(reader_thread),
                writer,
                child,
                incoming,
            })
            .map_err(|e| format!("failed to register spawned process: {e}"))
    }

    /// `CommandBuilder::new_default_prog().get_shell()` is `portable_pty`'s
    /// own cross-platform shell-resolution logic (Unix: `$SHELL`, falling
    /// back to the password database — i.e. whatever `chsh` set; Windows:
    /// its own default-shell lookup) — reused here rather than
    /// reimplemented, so a guest asking for "the default shell" gets
    /// exactly what a real terminal emulator would give it.
    fn default_shell(&mut self) -> String {
        CommandBuilder::new_default_prog().get_shell()
    }
}

/// One extension the kernel was configured to load via
/// `--load-extension` at startup (see
/// `KernelApp::on_before_command_line_processing`) — settled by
/// hands-on CEF spiking in `cef-extension-spike`: CEF's own dynamic,
/// per-context extension-loading embedder API was removed around the
/// versions this project targets, so the command-line switch at
/// process startup is the only way left to load one at all, which
/// means adding/removing an extension can never take effect live —
/// only on the next restart (see `add_extension`/`remove_extension`
/// below). Loaded from the persisted config file (see
/// `load_configured_extensions`) — editable by hand, but normally
/// built up via the "extensions" Facet's add/remove buttons, fed by
/// [`discover_extension_candidates`] so a user never has to locate or
/// type a path themselves for anything already installed in another
/// browser on their machine.
#[derive(Clone, Serialize, Deserialize)]
pub struct BrowserExtensionConfig {
    pub path: PathBuf,
    /// Fixed by the `key` field in the extension's own `manifest.json`
    /// — every extension this kernel supports needs one. Extensions
    /// without a `key` get an id Chromium derives from a hash of their
    /// path instead, which this project deliberately doesn't try to
    /// replicate (no way to verify it matches CEF's own computation
    /// short of trial and error) — [`discover_extension_candidates`]
    /// skips anything missing a `key`, and manually-added extensions
    /// need one too.
    pub id: String,
    pub name: String,
    /// Relative to the extension's root — from its own
    /// `manifest.json`'s `action.default_popup`, e.g.
    /// `"popup/index.html"` for Bitwarden. `None` for an extension
    /// with no popup UI at all (still loadable — `toggle_extension_popup`
    /// just has nothing to do for it).
    pub popup_page: Option<String>,
}

/// One extension found already installed in some other Chromium-based
/// browser on this machine (see [`discover_extension_candidates`]),
/// not yet added to this kernel's own [`BrowserExtensionConfig`] list
/// — offered as a one-click "Add" in the "extensions" Facet instead of
/// requiring the user to locate/type a path, which is the actual
/// problem this whole discovery mechanism exists to avoid.
#[derive(Clone)]
pub struct ExtensionCandidate {
    pub id: String,
    pub name: String,
    pub source_browser: String,
    pub popup_page: Option<String>,
    pub path: PathBuf,
}

/// Known macOS Chromium-based browser profile roots to scan for
/// already-installed extensions under `<root>/Default/Extensions/
/// <extension-id>/<version>/` — the same set manually confirmed during
/// this project's own CEF extension spike (Chrome, Brave, Edge,
/// Helium, and Comet all had Bitwarden installed under one of these on
/// the machine this was developed on). Not exhaustive (other profile
/// directories like "Profile 1", other OSes, other browsers entirely)
/// — covers the common case without trying to be a universal browser
/// detector.
#[cfg(target_os = "macos")]
const BROWSER_PROFILE_ROOTS: &[(&str, &str)] = &[
    ("Chrome", "Google/Chrome/Default/Extensions"),
    ("Chrome Canary", "Google/Chrome Canary/Default/Extensions"),
    ("Brave", "BraveSoftware/Brave-Browser/Default/Extensions"),
    ("Edge", "Microsoft Edge/Default/Extensions"),
    ("Helium", "net.imput.helium/Default/Extensions"),
    ("Comet", "Comet/Default/Extensions"),
];

/// Scans every known browser profile location (see
/// `BROWSER_PROFILE_ROOTS`) for already-installed extensions with a
/// `key` in their manifest — anything without one is skipped rather
/// than guessing at Chromium's path-hash id-derivation algorithm (see
/// `BrowserExtensionConfig::id`'s doc comment).
#[cfg(target_os = "macos")]
fn discover_extension_candidates() -> Vec<ExtensionCandidate> {
    let Some(base_dirs) = directories::BaseDirs::new() else { return Vec::new() };
    let app_support = base_dirs.home_dir().join("Library/Application Support");

    let mut candidates = Vec::new();
    for (browser_name, relative) in BROWSER_PROFILE_ROOTS {
        let extensions_dir = app_support.join(relative);
        let Ok(entries) = std::fs::read_dir(&extensions_dir) else { continue };
        for entry in entries.flatten() {
            let id_dir = entry.path();
            if !id_dir.is_dir() {
                continue;
            }
            let Some(id) = id_dir.file_name().and_then(|n| n.to_str()) else { continue };
            let Some(version_dir) = latest_version_dir(&id_dir) else { continue };
            let Some((name, popup_page)) = read_keyed_extension_manifest(&version_dir) else { continue };

            candidates.push(ExtensionCandidate {
                id: id.to_string(),
                name,
                source_browser: browser_name.to_string(),
                popup_page,
                path: version_dir,
            });
        }
    }
    candidates
}

#[cfg(not(target_os = "macos"))]
fn discover_extension_candidates() -> Vec<ExtensionCandidate> {
    Vec::new()
}

/// An extension's own directory can hold more than one version
/// subdirectory (Chromium prunes old ones but not always
/// immediately) — sorting lexically and taking the last one is a
/// reasonable approximation of "newest" without a real version-string
/// comparison, which isn't worth the complexity for this.
#[cfg(target_os = "macos")]
fn latest_version_dir(id_dir: &Path) -> Option<PathBuf> {
    let mut versions: Vec<PathBuf> = std::fs::read_dir(id_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    versions.pop()
}

/// Reads `manifest.json` from `version_dir`, returning `None` if
/// there's no `key` field (see `BrowserExtensionConfig::id`'s doc
/// comment for why) or if it's Manifest V2 — confirmed earlier (via
/// `cef-extension-spike`) that this CEF build rejects MV2 outright
/// ("Cannot install extension because it uses an unsupported manifest
/// version"), so offering one as a candidate at all would just walk
/// the user into that dead end after already clicking Add and
/// restarting. Resolves a Chrome i18n placeholder name like
/// `__MSG_extName__` against `_locales/<default_locale>/messages.json`
/// — Bitwarden's own `name` field is exactly this, not a literal
/// string, so without resolving it the discovered extension would
/// show up labeled `"__MSG_extName__"` instead of "Bitwarden".
#[cfg(target_os = "macos")]
fn read_keyed_extension_manifest(version_dir: &Path) -> Option<(String, Option<String>)> {
    let text = std::fs::read_to_string(version_dir.join("manifest.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;

    manifest.get("key")?.as_str()?;
    if manifest.get("manifest_version").and_then(|v| v.as_i64()) != Some(3) {
        return None;
    }

    let raw_name = manifest.get("name")?.as_str()?;
    let default_locale = manifest.get("default_locale").and_then(|v| v.as_str());
    let name = resolve_i18n_message(raw_name, version_dir, default_locale);

    let popup_page = manifest
        .get("action")
        .and_then(|a| a.get("default_popup"))
        .and_then(|p| p.as_str())
        .map(|s| s.to_string());

    Some((name, popup_page))
}

#[cfg(target_os = "macos")]
fn resolve_i18n_message(raw: &str, version_dir: &Path, default_locale: Option<&str>) -> String {
    let Some(key) = raw.strip_prefix("__MSG_").and_then(|s| s.strip_suffix("__")) else {
        return raw.to_string();
    };
    let locale = default_locale.unwrap_or("en");
    let messages_path = version_dir.join("_locales").join(locale).join("messages.json");
    let Ok(text) = std::fs::read_to_string(&messages_path) else { return raw.to_string() };
    let Ok(messages) = serde_json::from_str::<serde_json::Value>(&text) else { return raw.to_string() };
    messages
        .as_object()
        .and_then(|obj| obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)))
        .and_then(|(_, v)| v.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| raw.to_string())
}

#[derive(Serialize, Deserialize)]
struct ExtensionConfigFile {
    extensions: Vec<BrowserExtensionConfig>,
}

fn extensions_config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "rashomon", "rashomon")
        .context("could not determine a data directory for this OS/user")?;
    let data_dir = dirs.data_dir();
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data directory {}", data_dir.display()))?;
    Ok(data_dir.join("extensions.json"))
}

/// Empty (not an error) if the file doesn't exist yet — a fresh
/// install has configured zero extensions until the "extensions"
/// Facet's "Add" button (or hand-editing the file) puts some there.
fn load_configured_extensions() -> Result<Vec<BrowserExtensionConfig>> {
    let path = extensions_config_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let file: ExtensionConfigFile =
        serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(file.extensions)
}

fn save_configured_extensions(extensions: &[BrowserExtensionConfig]) -> Result<()> {
    let path = extensions_config_path()?;
    let file = ExtensionConfigFile { extensions: extensions.to_vec() };
    let text = serde_json::to_string_pretty(&file).context("failed to serialize extension config")?;
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// Re-checks a *configured* extension's manifest version at load
/// time, not just at discovery time — so one that was added before
/// this check existed (true for anyone who hit the "unsupported
/// manifest version" warning before this existed), or whose source
/// browser later updated it to MV2 for some reason, doesn't keep
/// getting silently handed to `--load-extension` and rejected by CEF
/// on every single startup.
fn configured_extension_is_valid(config: &BrowserExtensionConfig) -> bool {
    let Ok(text) = std::fs::read_to_string(config.path.join("manifest.json")) else { return false };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    manifest.get("manifest_version").and_then(|v| v.as_i64()) == Some(3)
}

/// Tracks one extension's popup lifecycle. `Opening` exists because
/// `browser_host_create_browser` doesn't create the `Browser`
/// synchronously (`on_after_created` fires later) — without it,
/// `toggle_extension_popup` called twice in quick succession would see
/// "nothing open yet" both times and open two popups instead of
/// open-then-close. Ported directly from the design proven in
/// `cef-extension-spike`.
enum PopupState {
    Closed,
    Opening,
    Open(i32, Browser),
}

struct ExtensionRuntime {
    config: BrowserExtensionConfig,
    popup: Arc<Mutex<PopupState>>,
}

wrap_life_span_handler! {
    struct ExtensionPopupLifeSpanHandler {
        tracked: Arc<Mutex<PopupState>>,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            let id = browser.identifier();
            *self.tracked.lock().expect("popup lock poisoned") = PopupState::Open(id, browser.clone());
        }

        /// Only clears to `Closed` if this is still the browser the
        /// shared state thinks is open — if the popup was already
        /// closed and reopened before this particular close finishes
        /// landing, this must not clobber that newer state.
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            let closing_id = browser.identifier();
            let mut state = self.tracked.lock().expect("popup lock poisoned");
            if let PopupState::Open(open_id, _) = &*state {
                if *open_id == closing_id {
                    *state = PopupState::Closed;
                }
            }
        }
    }
}

wrap_client! {
    struct ExtensionPopupClient {
        tracked: Arc<Mutex<PopupState>>,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(ExtensionPopupLifeSpanHandler::new(self.tracked.clone()))
        }
    }
}

/// The WIT-facing path for `HostBrowserContext::toggle_extension_popup`
/// (called from a Facet Component) — reproduces the exact
/// open/close-race-safe toggle `cef-extension-spike` validated.
fn toggle_extension_popup_impl(ext: &ExtensionRuntime) -> Result<(), String> {
    let mut state = ext.popup.lock().expect("popup lock poisoned");
    match &*state {
        PopupState::Open(_, browser) => {
            let browser = browser.clone();
            *state = PopupState::Closed;
            drop(state);
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
            Ok(())
        }
        PopupState::Opening => Err("popup already opening".to_string()),
        PopupState::Closed => {
            let Some(popup_page) = &ext.config.popup_page else {
                drop(state);
                return Err(format!("{} has no popup page", ext.config.name));
            };
            *state = PopupState::Opening;
            drop(state);
            let mut popup_client = ExtensionPopupClient::new(ext.popup.clone());
            let window_info = WindowInfo {
                runtime_style: RuntimeStyle::ALLOY,
                ..Default::default()
            };
            let settings = BrowserSettings::default();
            let url =
                CefString::from(format!("chrome-extension://{}/{}", ext.config.id, popup_page).as_str());
            browser_host_create_browser(
                Some(&window_info),
                Some(&mut popup_client),
                Some(&url),
                Some(&settings),
                None,
                None,
            );
            Ok(())
        }
    }
}

/// There's exactly one of these for now — the kernel's one user
/// profile (see the WIT doc comment on `browser-context`).
pub struct BrowserContext {
    id: String,
}

/// A real classic (non-Views) CEF `Browser`, embedded as a child of the
/// one native `winit` window via `WindowInfo::set_as_child` — never in
/// its own dedicated top-level window. `create_tab` mounts every tab
/// into the shared switcher's tab list (see [`BrowserSwitcherState`]),
/// the same one-window design `cef-extension-spike`/`cef-winit-spike`
/// validated.
pub struct BrowserTab {
    id: String,
    browser: Browser,
    title: Arc<Mutex<String>>,
}

/// Whether a switcher tab is a real website (`create_tab`, where an
/// address bar makes sense) or a Facet View (`Kernel::open_window` —
/// terminal/graph-view/extensions, where it doesn't: there's no URL a
/// user should be navigating, just `data:` HTML the host generated —
/// and where, on macOS, the tab is `wry`-hosted instead of CEF-hosted
/// so it can render transparently over the window's background blur;
/// see [`TabWidget`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabKind {
    Browser,
    FacetView,
}

/// The actual native widget backing one switcher tab — a real CEF
/// `Browser` (classic-embedded, always opaque — correct for both real
/// websites and, on non-macOS, Facet Views too, since `wry`/the
/// background blur are macOS-only for now), or, on macOS, a
/// transparent `wry` webview for Facet Views/the sidebar.
enum TabWidget {
    Browser(Browser),
    #[cfg(target_os = "macos")]
    Facet(mac::FacetWebView),
}

/// One switcher tab's full bookkeeping: its id, whether it's a real
/// website or a Facet View (decides urlbar visibility), the live
/// `document.title` tracker (`TabDisplayHandler`/`wry`'s
/// `document_title_changed_handler` both feed the same `Arc`), and the
/// actual native widget.
struct SwitcherTab {
    id: String,
    kind: TabKind,
    title: Arc<Mutex<String>>,
    widget: TabWidget,
}

/// All of this kernel's windowing state: the one `winit`-owned native
/// window every tab (browser or Facet View) and the sidebar/urlbar
/// attach to, plus the switcher's tab list. Replaces the CEF
/// Views-framework design (`Panel`/`BoxLayout`/`BrowserView`) validated
/// in `crates/cef-extension-spike` — see the module doc comment for
/// why: CEF's `BrowserView` can never be transparent, which a
/// translucent sidebar needs. `crates/cef-winit-spike` validated the
/// replacement this now ports: `winit` owns the window, CEF embeds
/// classic (non-Views) child browsers into it, `wry` hosts everything
/// that needs to be transparent.
struct BrowserSwitcherState {
    /// The raw native parent handle `wry::WebViewBuilder::build_as_child`
    /// wants — only meaningful on macOS, where `wry` is wired up at all
    /// (see `mac::FacetWebView`/`mac::UrlBarWebView`).
    #[cfg(target_os = "macos")]
    native_window_handle: mac::NativeWindowHandle,
    /// The same native window, reinterpreted as the `cef_window_handle_t`
    /// CEF's `WindowInfo::set_as_child` wants — needed on every
    /// platform, since real browser tabs are always CEF-embedded.
    native_parent: cef::sys::cef_window_handle_t,
    /// Host browser-chrome, not a Facet — see [`mac::UrlBarWebView`]'s
    /// doc comment. `None` on non-macOS, where this integration isn't
    /// wired up yet.
    #[cfg(target_os = "macos")]
    urlbar: mac::UrlBarWebView,
    /// Exposed to guests via `list-tabs`/`switch-to-tab` so a sidebar
    /// Facet can address a tab without ever holding (or being handed)
    /// its `browser-tab` resource.
    tabs: Vec<SwitcherTab>,
    active_index: usize,
    /// The sidebar's actual content, once [`Kernel::open_sidebar_view`]
    /// creates it — `None` until then. Kept here (not just a local in
    /// that function) so it isn't dropped/torn down immediately after
    /// creation. A [`TabWidget::Facet`] (transparent `wry`) on macOS,
    /// a [`TabWidget::Browser`] (opaque, classic-embedded CEF) on every
    /// other platform, where `wry`/the background blur aren't wired up
    /// yet — see the module doc comment's "macOS-only for now" scoping.
    sidebar_widget: Option<TabWidget>,
    /// The window's current logical size — starts at
    /// `WINDOW_WIDTH_PX`/`WINDOW_HEIGHT_PX` (what it's actually
    /// created at) and kept live-updated on every
    /// `WindowEvent::Resized` (see [`WinitKernelApp::window_event`]),
    /// so every rect method below reflects the window's real current
    /// size instead of baking in its size at creation time.
    window_width: i32,
    window_height: i32,
}

// `native_parent` is a raw `cef_window_handle_t` (`*mut c_void` on
// macOS) — no automatic `Send`/`Sync`, asserted manually here under
// the same invariant `mac::NativeWindowHandle` already relies on:
// everything touching it stays on CEF/winit's one UI thread,
// synchronized through `Arc<Mutex<_>>` at this very type. Required for
// `KernelState`/`InputBridge` to satisfy `wasmtime_wasi::WasiView:
// Send` and `cef::wrapper::message_router::BrowserSideHandler: Send +
// Sync`.
unsafe impl Send for BrowserSwitcherState {}
unsafe impl Sync for BrowserSwitcherState {}

impl BrowserSwitcherState {
    fn switch_to(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if index != self.active_index {
            if let Some(prev) = self.tabs.get(self.active_index) {
                hide_widget(&prev.widget);
            }
            self.active_index = index;
        }
        self.layout_active();
    }

    fn switch_to_id(&mut self, tab_id: &str) -> Result<(), String> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .ok_or_else(|| format!("no such tab: {tab_id}"))?;
        self.switch_to(index);
        Ok(())
    }

    /// Positions/shows the currently active tab's widget at the
    /// content rect appropriate for its kind (leaving room for the
    /// urlbar above it iff it's a `TabKind::Browser` tab), and syncs
    /// the urlbar's own visibility/text to match. Called whenever the
    /// active tab changes, and once right after the very first tab
    /// mounts.
    fn layout_active(&mut self) {
        let Some(tab) = self.tabs.get(self.active_index) else { return };
        let show_urlbar = tab.kind == TabKind::Browser;
        let rect = if show_urlbar { self.content_rect_below_urlbar() } else { self.content_rect_full() };
        show_widget_at(&tab.widget, rect);

        #[cfg(target_os = "macos")]
        {
            self.urlbar.set_visible(show_urlbar);
            if show_urlbar {
                if let TabWidget::Browser(browser) = &tab.widget {
                    self.urlbar.set_text(&browser_url(browser));
                }
            }
        }
    }

    fn sidebar_rect(&self) -> Rect {
        sidebar_rect(self.window_width, self.window_height)
    }

    fn urlbar_rect(&self) -> Rect {
        urlbar_rect(self.window_width)
    }

    fn content_rect_full(&self) -> Rect {
        content_rect_full(self.window_width, self.window_height)
    }

    fn content_rect_below_urlbar(&self) -> Rect {
        content_rect_below_urlbar(self.window_width, self.window_height)
    }

    /// Called from [`WinitKernelApp::window_event`] on every
    /// `WindowEvent::Resized` — updates the live window size the rect
    /// methods above compute from, then repositions everything that
    /// isn't already repositioned some other way: the sidebar and
    /// urlbar (fixed slots, not part of the switchable tab list) and
    /// whichever tab is currently active (every other tab is hidden,
    /// and gets its bounds brought up to date for free the next time
    /// it's switched to — see `layout_active`).
    fn resize_window(&mut self, width: i32, height: i32) {
        self.window_width = width;
        self.window_height = height;

        #[cfg(target_os = "macos")]
        self.urlbar.set_bounds(cef_rect_to_wry(self.urlbar_rect()));

        if let Some(sidebar) = &self.sidebar_widget {
            show_widget_at(sidebar, self.sidebar_rect());
        }

        self.layout_active();
    }

    /// Navigates whichever tab is currently active — only meaningful
    /// for `TabKind::Browser` tabs (a `TabWidget::Facet` has no
    /// navigable `Browser` at all); silently a no-op otherwise, same
    /// as the urlbar being hidden for those tabs in the first place.
    fn navigate_active(&self, url: &str) {
        let Some(tab) = self.tabs.get(self.active_index) else { return };
        if let TabWidget::Browser(browser) = &tab.widget {
            if let Some(frame) = browser.main_frame() {
                frame.load_url(Some(&CefString::from(url)));
            }
        }
    }
}

/// Hides a tab's native widget without destroying it — used when
/// switching away from it, so it keeps running (a terminal session's
/// PTY, a page's JS timers) rather than being torn down and recreated
/// on every switch.
fn hide_widget(widget: &TabWidget) {
    match widget {
        TabWidget::Browser(browser) => {
            #[cfg(target_os = "macos")]
            if let Some(host) = browser.host() {
                let handle = host.window_handle();
                if !handle.is_null() {
                    mac::set_view_hidden(handle as *mut std::ffi::c_void, true);
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                // Hiding a classic-embedded CEF child browser's native
                // view needs a platform call (Win32/Xlib) this hasn't
                // been ported to yet — see the module doc comment's
                // "macOS-only for now" scoping. Tabs stay stacked/all
                // visible on non-macOS until this is ported.
                let _ = browser;
            }
        }
        #[cfg(target_os = "macos")]
        TabWidget::Facet(webview) => webview.set_visible(false),
    }
}

/// Resizes, repositions, and shows a tab's native widget — used both
/// for the first time a tab mounts and every time it becomes active
/// again after being hidden.
fn show_widget_at(widget: &TabWidget, rect: Rect) {
    match widget {
        TabWidget::Browser(browser) => {
            #[cfg(target_os = "macos")]
            if let Some(host) = browser.host() {
                let handle = host.window_handle();
                if !handle.is_null() {
                    mac::set_view_frame(handle as *mut std::ffi::c_void, rect);
                    mac::set_view_hidden(handle as *mut std::ffi::c_void, false);
                }
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (browser, rect);
        }
        #[cfg(target_os = "macos")]
        TabWidget::Facet(webview) => {
            webview.set_bounds(cef_rect_to_wry(rect));
            webview.set_visible(true);
        }
    }
}

/// Registers a tab in the shared switcher under `tab_id`, making it
/// the active (visible) one immediately if it's the very first tab
/// ever created, otherwise mounting it hidden — only one tab is ever
/// visible at a time, toggled by [`BrowserSwitcherState::switch_to`].
/// The sidebar Facet discovers new tabs itself by polling `list-tabs`,
/// rather than being told about each one as it's created. Shared by
/// `create_tab` (the `rashomon:browser` Host) and `Kernel::open_window`
/// (Facet Views) — both just hand this whichever [`TabWidget`] they
/// created, so a terminal session and a browser tab become
/// indistinguishable switcher entries, titles included.
fn mount_tab(
    tab_id: String,
    browser_switcher: &Arc<Mutex<Option<BrowserSwitcherState>>>,
    widget: TabWidget,
    title: Arc<Mutex<String>>,
    kind: TabKind,
) {
    let mut guard = browser_switcher.lock().expect("browser switcher lock poisoned");
    let Some(switcher) = guard.as_mut() else {
        eprintln!("mount-tab: no browser switcher window yet — tab created but not mounted anywhere");
        return;
    };

    let index = switcher.tabs.len();
    switcher.tabs.push(SwitcherTab { id: tab_id, kind, title, widget });
    if index == 0 {
        switcher.active_index = 0;
        switcher.layout_active();
    } else {
        hide_widget(&switcher.tabs[index].widget);
    }
}

wrap_task! {
    struct SwitchTabTask {
        browser_switcher: Arc<Mutex<Option<BrowserSwitcherState>>>,
        tab_id: String,
    }

    impl Task {
        fn execute(&self) {
            let mut switcher = self.browser_switcher.lock().expect("browser switcher lock poisoned");
            if let Some(switcher) = switcher.as_mut() {
                if let Err(e) = switcher.switch_to_id(&self.tab_id) {
                    eprintln!("switch-to-tab: {e}");
                }
            }
        }
    }
}

/// Shared by `BrowserSwitcherState::layout_active`/`navigate_active`
/// and `HostBrowserContext::list_tabs`/`HostBrowserTab` — the one
/// place that knows how to pull a live URL back out of a `Browser`.
fn browser_url(browser: &Browser) -> String {
    browser
        .main_frame()
        .map(|f| CefString::from(&f.url()).to_string())
        .unwrap_or_default()
}

/// Adds a scheme if the user typed something scheme-less (e.g.
/// `example.com` or a bare search term) — the same bare-minimum
/// heuristic every address bar needs, without trying to distinguish a
/// real domain from a search query (there's no search engine wired up
/// here to fall back to).
fn normalize_url_input(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.contains("://") {
        return trimmed.to_string();
    }
    if looks_like_url(trimmed) {
        return format!("https://{trimmed}");
    }
    // Anything that doesn't look like an address (a plain search term,
    // or anything with a space in it) goes to Google search instead of
    // being malformed into `https://<query with spaces>`.
    let encoded = CefString::from(&uriencode(Some(&CefString::from(trimmed)), 1)).to_string();
    format!("https://www.google.com/search?q={encoded}")
}

/// A rough, browser-style heuristic for telling a real address apart
/// from a search query: no spaces, and either `localhost`, a bare IP,
/// or a dotted hostname (ignoring a trailing `:port` or `/path`).
/// Deliberately simple rather than a full URL parser — the cost of a
/// wrong guess is just falling through to a Google search either way.
fn looks_like_url(input: &str) -> bool {
    if input.is_empty() || input.contains(' ') {
        return false;
    }
    let host_part = input.split('/').next().unwrap_or(input).split(':').next().unwrap_or(input);
    if host_part.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if host_part.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    host_part.contains('.') && !host_part.starts_with('.') && !host_part.ends_with('.')
}

/// The one native window's size at creation — `WinitKernelApp::resumed`
/// still opens it at exactly this size, but (unlike before) the window
/// is resizable and every rect function below takes the *current*
/// width/height as parameters rather than closing over these directly,
/// so resizing the real window actually relayouts everything instead
/// of just leaving stale bounds from whatever size it started at — see
/// `BrowserSwitcherState::resize_window`.
const WINDOW_WIDTH_PX: i32 = 1280;
const WINDOW_HEIGHT_PX: i32 = 800;
/// Width of the sidebar region on the left — the rest of the window is
/// the content region every switcher tab mounts into.
const SIDEBAR_WIDTH_PX: i32 = 225;
/// Height of the urlbar strip, reserved at the top of the content
/// region only while a `TabKind::Browser` tab is active.
const URLBAR_HEIGHT_PX: i32 = 36;

fn sidebar_rect(_window_width: i32, window_height: i32) -> Rect {
    Rect { x: 0, y: 0, width: SIDEBAR_WIDTH_PX, height: window_height }
}

fn urlbar_rect(window_width: i32) -> Rect {
    Rect { x: SIDEBAR_WIDTH_PX, y: 0, width: window_width - SIDEBAR_WIDTH_PX, height: URLBAR_HEIGHT_PX }
}

fn content_rect_full(window_width: i32, window_height: i32) -> Rect {
    Rect { x: SIDEBAR_WIDTH_PX, y: 0, width: window_width - SIDEBAR_WIDTH_PX, height: window_height }
}

fn content_rect_below_urlbar(window_width: i32, window_height: i32) -> Rect {
    Rect {
        x: SIDEBAR_WIDTH_PX,
        y: URLBAR_HEIGHT_PX,
        width: window_width - SIDEBAR_WIDTH_PX,
        height: window_height - URLBAR_HEIGHT_PX,
    }
}

// Used by `ViewClient` (Facet Views embedded via a classic CEF browser
// — the non-macOS fallback only; macOS hosts Facet Views via `wry`
// instead, with no CEF `DisplayHandler` involved at all) — title
// tracking only, deliberately *not* the page-visit tracking
// `TabDisplayHandler` below also does for real browser tabs, since a
// Facet View's "URL" is just its `data:` HTML payload, not a real page
// worth recording in the graph.
wrap_display_handler! {
    struct TitleOnlyDisplayHandler {
        title: Arc<Mutex<String>>,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            *self.title.lock().expect("title lock poisoned") =
                title.map(|t| t.to_string()).unwrap_or_default();
        }
    }
}

// `visited_url` (below) is this tab's own dedup state: the last URL
// it's already recorded (via `record_page_visit`) — `None` initially,
// so the very first navigation always records. Lives for exactly this
// `Browser`'s lifetime: once it's closed, this (and the `Arc` holding
// it) is dropped along with the rest of this `TabClient`/
// `TabDisplayHandler` pair, so a brand new tab later navigating to the
// same URL has fresh dedup state and records again — the "once per
// session unless manually closed and reopened" policy, enforced
// simply by tying the dedup state's lifetime to the tab's own. (A
// plain `//` comment, not `///` — this macro's own parser doesn't
// accept doc-comment attributes between fields.)
wrap_display_handler! {
    struct TabDisplayHandler {
        title: Arc<Mutex<String>>,
        bridge: Arc<Mutex<InputBridge>>,
        visited_url: Arc<Mutex<Option<String>>>,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            *self.title.lock().expect("title lock poisoned") =
                title.map(|t| t.to_string()).unwrap_or_default();
        }

        /// Fires for every real navigation of this tab's main frame —
        /// filtered to the main frame specifically (`frame.is_main()`)
        /// so an embedded iframe's own, unrelated navigation doesn't
        /// get mistaken for "this tab navigated somewhere new." Dedupes
        /// repeat arrivals at the *same* URL (e.g. a reload, or
        /// navigating away and back) against `visited_url` before
        /// handing off to [`record_page_visit`] — without this, every
        /// single reload of an already-visited page would record
        /// another Occurrence, which isn't "a further navigation" in
        /// any meaningful sense.
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            let Some(frame) = frame else { return };
            if frame.is_main() == 0 {
                return;
            }
            let Some(url) = url else { return };
            let url = url.to_string();

            let mut visited = self.visited_url.lock().expect("visited url lock poisoned");
            if visited.as_deref() == Some(url.as_str()) {
                return;
            }
            *visited = Some(url.clone());
            drop(visited);

            record_page_visit(&self.bridge, &url);
        }
    }
}

wrap_client! {
    struct TabClient {
        title: Arc<Mutex<String>>,
        bridge: Arc<Mutex<InputBridge>>,
        visited_url: Arc<Mutex<Option<String>>>,
    }

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(TabDisplayHandler::new(self.title.clone(), self.bridge.clone(), self.visited_url.clone()))
        }
    }
}

impl rashomon::browser::types::Host for KernelState {}

impl rashomon::browser::types::HostBrowserContext for KernelState {
    fn id(&mut self, self_: Resource<BrowserContext>) -> String {
        self.table.get(&self_).map(|c| c.id.clone()).unwrap_or_default()
    }

    fn list_extensions(
        &mut self,
        _self_: Resource<BrowserContext>,
    ) -> Vec<rashomon::browser::types::ExtensionInfo> {
        let mut extensions: Vec<_> = self
            .extensions
            .iter()
            .map(|ext| rashomon::browser::types::ExtensionInfo {
                id: ext.config.id.clone(),
                name: ext.config.name.clone(),
                popup_open: matches!(
                    *ext.popup.lock().expect("popup lock poisoned"),
                    PopupState::Open(..)
                ),
            })
            .collect();
        extensions.sort_by(|a, b| a.name.cmp(&b.name));
        extensions
    }

    fn toggle_extension_popup(
        &mut self,
        _self_: Resource<BrowserContext>,
        extension_id: String,
    ) -> Result<(), String> {
        let ext = self
            .extensions
            .iter()
            .find(|ext| ext.config.id == extension_id)
            .ok_or_else(|| format!("unknown extension id: {extension_id}"))?;
        toggle_extension_popup_impl(ext)
    }

    fn list_tabs(&mut self, _self_: Resource<BrowserContext>) -> Vec<rashomon::browser::types::TabInfo> {
        let switcher = self.browser_switcher.lock().expect("browser switcher lock poisoned");
        let Some(switcher) = switcher.as_ref() else { return Vec::new() };
        switcher
            .tabs
            .iter()
            .map(|tab| {
                // Only a `TabWidget::Browser` has a real navigable URL
                // — a `TabWidget::Facet` (terminal/graph-view/
                // extensions) doesn't, so it falls back to its tracked
                // title alone.
                let url = match &tab.widget {
                    TabWidget::Browser(browser) => browser_url(browser),
                    #[cfg(target_os = "macos")]
                    TabWidget::Facet(_) => String::new(),
                };
                // The real tracked `document.title`, falling back to
                // the url for the brief window before the page's own
                // title has fired (or for a page that never sets one).
                let tracked = tab.title.lock().expect("title lock poisoned").clone();
                let title = if tracked.is_empty() { url.clone() } else { tracked };
                rashomon::browser::types::TabInfo { id: tab.id.clone(), url, title }
            })
            .collect()
    }

    /// Defers the actual view-tree mutation via `post_task` instead of
    /// switching synchronously — this is called from inside
    /// `InputQueryHandler::on_query_str`'s `call_handle_input`, itself
    /// running with `InputBridge`'s mutex already held; mutating
    /// `active_region` (`add_child_view`/`remove_child_view`) from
    /// there froze the app outright (confirmed empirically), almost
    /// certainly a reentrant callback trying to re-lock the same
    /// (non-reentrant) mutex on the same thread. Posting to the UI
    /// thread lets the current call stack (and that lock) unwind
    /// first — the same fix `cef-extension-spike` needed for a
    /// different reentrant Views-tree mutation earlier in this
    /// project. Returns `Ok` unconditionally since the actual result
    /// is only known once the deferred task runs; any failure there
    /// is logged, not surfaced to the caller.
    fn switch_to_tab(&mut self, _self_: Resource<BrowserContext>, tab_id: String) -> Result<(), String> {
        let mut task = SwitchTabTask::new(self.browser_switcher.clone(), tab_id);
        post_task(ThreadId::UI, Some(&mut task));
        Ok(())
    }

    /// Filters against the *persisted config file*, not `self.extensions`
    /// (the live, active-this-session list, fixed since startup) — so
    /// adding or removing an extension is reflected here on the very
    /// next poll, rather than only after a restart. `self.extensions`
    /// not changing until restart is still correct for `list_extensions`
    /// itself (it reflects what's actually loaded right now); this is
    /// specifically about which candidates are still worth offering.
    fn list_extension_candidates(
        &mut self,
        _self_: Resource<BrowserContext>,
    ) -> Vec<rashomon::browser::types::ExtensionCandidate> {
        let configured_ids: std::collections::HashSet<String> = load_configured_extensions()
            .unwrap_or_default()
            .into_iter()
            .map(|ext| ext.id)
            .collect();
        let mut candidates: Vec<_> = self
            .extension_candidates
            .iter()
            .filter(|candidate| !configured_ids.contains(&candidate.id))
            .map(|candidate| rashomon::browser::types::ExtensionCandidate {
                id: candidate.id.clone(),
                name: candidate.name.clone(),
                source_browser: candidate.source_browser.clone(),
            })
            .collect();
        candidates.sort_by(|a, b| a.name.cmp(&b.name));
        candidates
    }

    fn add_extension(&mut self, _self_: Resource<BrowserContext>, candidate_id: String) -> Result<(), String> {
        let candidate = self
            .extension_candidates
            .iter()
            .find(|candidate| candidate.id == candidate_id)
            .ok_or_else(|| format!("unknown candidate id: {candidate_id}"))?;

        let mut configured = load_configured_extensions().map_err(|e| e.to_string())?;
        if configured.iter().any(|ext| ext.id == candidate.id) {
            return Err(format!("{} is already added", candidate.name));
        }
        configured.push(BrowserExtensionConfig {
            path: candidate.path.clone(),
            id: candidate.id.clone(),
            name: candidate.name.clone(),
            popup_page: candidate.popup_page.clone(),
        });
        save_configured_extensions(&configured).map_err(|e| e.to_string())
    }

    fn remove_extension(&mut self, _self_: Resource<BrowserContext>, extension_id: String) -> Result<(), String> {
        let mut configured = load_configured_extensions().map_err(|e| e.to_string())?;
        let original_len = configured.len();
        configured.retain(|ext| ext.id != extension_id);
        if configured.len() == original_len {
            return Err(format!("not configured: {extension_id}"));
        }
        save_configured_extensions(&configured).map_err(|e| e.to_string())
    }

    fn drop(&mut self, self_: Resource<BrowserContext>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl rashomon::browser::types::HostBrowserTab for KernelState {
    fn id(&mut self, self_: Resource<BrowserTab>) -> String {
        self.table.get(&self_).map(|tab| tab.id.clone()).unwrap_or_default()
    }

    fn navigate(&mut self, self_: Resource<BrowserTab>, url: String) -> Result<(), String> {
        let tab = self.table.get(&self_).map_err(|e| e.to_string())?;
        let frame = tab.browser.main_frame().ok_or("tab has no main frame")?;
        frame.load_url(Some(&CefString::from(url.as_str())));
        Ok(())
    }

    fn current_url(&mut self, self_: Resource<BrowserTab>) -> String {
        let Ok(tab) = self.table.get(&self_) else { return String::new() };
        browser_url(&tab.browser)
    }

    fn title(&mut self, self_: Resource<BrowserTab>) -> String {
        let Ok(tab) = self.table.get(&self_) else { return String::new() };
        tab.title.lock().expect("title lock poisoned").clone()
    }

    fn close(&mut self, self_: Resource<BrowserTab>) {
        if let Ok(tab) = self.table.get(&self_) {
            if let Some(host) = tab.browser.host() {
                host.close_browser(1);
            }
        }
    }

    fn snapshot_dom(&mut self, _self_: Resource<BrowserTab>) -> Result<String, String> {
        Err("not implemented yet".to_string())
    }

    fn inject_script(&mut self, self_: Resource<BrowserTab>, script: String) -> Result<String, String> {
        let tab = self.table.get(&self_).map_err(|e| e.to_string())?;
        let frame = tab.browser.main_frame().ok_or("tab has no main frame")?;
        frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
        // `execute_java_script` has no return value in CEF's own API —
        // getting the script's result back would need a round trip
        // through a V8 handler/extension, same as the rest of this
        // kernel's input bridge. Deferred along with `snapshot-dom`.
        Ok(String::new())
    }

    fn anchor(&mut self, _self_: Resource<BrowserTab>, _spec: String) -> Result<String, String> {
        Err("not implemented yet".to_string())
    }

    fn drop(&mut self, self_: Resource<BrowserTab>) -> wasmtime::Result<()> {
        if let Ok(tab) = self.table.delete(self_) {
            if let Some(host) = tab.browser.host() {
                host.close_browser(1);
            }
        }
        Ok(())
    }
}

impl rashomon::browser::control::Host for KernelState {
    fn create_context(&mut self) -> Resource<BrowserContext> {
        self.table
            .push(BrowserContext { id: "default".to_string() })
            .expect("resource table push failed")
    }

    /// Embeds the new tab as a classic (non-Views) CEF child browser
    /// of the shared switcher window (making it visible immediately if
    /// it's the first tab ever created, exactly like
    /// `cef-extension-spike`'s `TabSwitcher` mounted its first tab at
    /// startup) rather than giving it a window of its own.
    /// `browser_host_create_browser_sync` (not the async
    /// `browser_host_create_browser`) is what makes a real `Browser`
    /// available immediately — needed here since `create-tab` has to
    /// hand one back as a WIT resource synchronously, with nowhere to
    /// stash an async `on_after_created` callback's result in the
    /// meantime.
    fn create_tab(&mut self, _context: Resource<BrowserContext>, url: String) -> Resource<BrowserTab> {
        let title = Arc::new(Mutex::new(String::new()));
        let bridge_handle = self
            .bridge_handle
            .clone()
            .expect("bridge_handle set immediately after InputBridge construction, before any Facet ever runs");
        let mut client = TabClient::new(title.clone(), bridge_handle, Arc::new(Mutex::new(None)));
        let native_parent_and_rect = self
            .browser_switcher
            .lock()
            .expect("browser switcher lock poisoned")
            .as_ref()
            .map(|s| (s.native_parent, s.content_rect_below_urlbar()));
        let window_info = native_parent_and_rect
            .map(|(parent, rect)| WindowInfo::default().set_as_child(parent, &rect))
            .unwrap_or_default();
        let settings = BrowserSettings::default();
        let cef_url = CefString::from(url.as_str());
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&cef_url),
            Some(&settings),
            None,
            None,
        )
        .expect("browser_host_create_browser_sync failed");

        let id = format!("tab-{}", next_tab_id());
        mount_tab(id.clone(), &self.browser_switcher, TabWidget::Browser(browser.clone()), title.clone(), TabKind::Browser);

        self.table
            .push(BrowserTab { id, browser, title })
            .expect("resource table push failed")
    }
}

/// Unique across the process, independent of the `ResourceTable`'s own
/// internal indices (which get reused once a tab's resource is
/// dropped) — the tab's identity shouldn't change meaning if an
/// unrelated, later tab happens to land in the same table slot.
fn next_tab_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn to_lib_role(role: rashomon::graph::types::Role) -> rashomon_graph::Role {
    match role {
        rashomon::graph::types::Role::Entity => rashomon_graph::Role::Entity,
        rashomon::graph::types::Role::Occurrence => rashomon_graph::Role::Occurrence,
    }
}

fn to_wit_node(node: rashomon_graph::Node) -> rashomon::graph::types::Node {
    rashomon::graph::types::Node {
        id: node.id,
        node_type: node.node_type,
        role: match node.role {
            rashomon_graph::Role::Entity => rashomon::graph::types::Role::Entity,
            rashomon_graph::Role::Occurrence => rashomon::graph::types::Role::Occurrence,
        },
        properties: node
            .properties
            .into_iter()
            .map(|(key, value)| rashomon::graph::types::Property { key, value })
            .collect(),
    }
}

fn to_wit_edge(edge: rashomon_graph::Edge) -> rashomon::graph::types::Edge {
    rashomon::graph::types::Edge {
        id: edge.id,
        edge_type: edge.edge_type,
        source: edge.source,
        target: edge.target,
        timestamp: edge.timestamp,
        confidence: edge.confidence,
    }
}

/// Builds `components/<name>` with `cargo component build` if its
/// compiled `.wasm` isn't already on disk, then returns the artifact
/// path.
fn ensure_component_built(name: &str) -> Result<PathBuf> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let component_dir = workspace_root.join("components").join(name);
    // Cargo normalizes hyphens in the crate name to underscores in
    // build artifact filenames (e.g. `terminal-xterm` -> `terminal_xterm.wasm`).
    let artifact_name = name.replace('-', "_");
    let wasm_path = component_dir
        .join("target/wasm32-wasip1/debug")
        .join(format!("{artifact_name}.wasm"));

    if !wasm_path.exists() {
        println!("{name} component not built yet; running `cargo component build`...");
        let manifest_path = component_dir.join("Cargo.toml");
        let status = Command::new("cargo")
            .args(["component", "build", "--manifest-path"])
            .arg(&manifest_path)
            .status()
            .context("failed to run `cargo component build` — is cargo-component installed?")?;
        if !status.success() {
            bail!("`cargo component build` for components/{name} failed");
        }
    }

    if !wasm_path.exists() {
        bail!(
            "expected component artifact at {} after build",
            wasm_path.display()
        );
    }

    Ok(wasm_path)
}

/// Path to the kernel's graph database, under the OS-appropriate data
/// directory (e.g. `~/Library/Application Support/dev.rashomon.rashomon`
/// on macOS, `~/.local/share/rashomon` on Linux) — created if it
/// doesn't exist yet.
fn graph_db_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("dev", "rashomon", "rashomon")
        .context("could not determine a data directory for this OS/user")?;
    let data_dir = dirs.data_dir();
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data directory {}", data_dir.display()))?;
    Ok(data_dir.join("graph.redb"))
}

/// Same `data:` URI trick `cefsimple`'s own handler uses for load-error
/// pages — base64 the raw bytes, then URI-encode that (via CEF's own
/// utilities, not hand-rolled escaping) so arbitrary HTML/JS content
/// (quotes, newlines, `#`) survives being embedded in a URI intact.
fn html_data_uri(html: &str) -> String {
    let b64 = CefString::from(&base64_encode(Some(html.as_bytes()))).to_string();
    let encoded = CefString::from(&uriencode(Some(&CefString::from(b64.as_str())), 0)).to_string();
    format!("data:text/html;base64,{encoded}")
}

/// The compiled form of every Facet this kernel knows how to open as a
/// View, plus the one `Linker` they're all instantiated against.
/// Compiling is the expensive part — this is built once at startup, and
/// every View is just a fresh, cheap `FacetWorld::instantiate` against
/// an already-compiled `Component` here.
struct FacetRegistry {
    linker: Linker<KernelState>,
    components: HashMap<String, Component>,
}

impl FacetRegistry {
    fn component(&self, facet_name: &str) -> Result<&Component> {
        self.components
            .get(facet_name)
            .ok_or_else(|| anyhow!("no registered facet named {facet_name:?}"))
    }
}

/// One live View: a Facet Component instance, plus enough state to let
/// more than one Window mirror it (see [`Kernel::open_window`]) without
/// either Window losing output to whichever one happens to poll first.
struct ViewHandle {
    bindings: FacetWorld,
    /// The HTML `render()` produced when this View was first opened.
    /// Reused verbatim (with a different `window.__rashomonWindowId`
    /// spliced in) for every later Window that mirrors this View,
    /// rather than calling `render()` again — `render()`'s own first
    /// call already drained whatever the Facet had buffered, so a
    /// second call would show a newly-opened mirror an empty terminal
    /// instead of the same history the first Window saw.
    initial_html: String,
    /// Output `poll-output` has drained but not yet delivered to each
    /// Window watching this View, keyed by window id. Every Window's
    /// own `__poll__` request appends freshly-drained bytes to *every*
    /// entry here (not just its own) before popping its own — that's
    /// what makes output arrive at every mirroring Window rather than
    /// being consumed once by whichever Window asked first.
    pending: HashMap<String, String>,
}

/// All of this kernel's live, mutable state in one place — shared
/// (behind one `Arc<Mutex<_>>`, owned by [`Kernel`]) between
/// [`InputQueryHandler`] (routes every `window.cefQuery` call — input or
/// a [`POLL_REQUEST`] — to the right View) and the life-span handler
/// that maintains `browser_count`.
pub(crate) struct InputBridge {
    store: Store<KernelState>,
    browser_count: u32,
    /// Every currently-open View, keyed by the id [`Kernel::open_view`]
    /// assigned it.
    views: HashMap<String, ViewHandle>,
    /// Which View each open Window is currently mirroring, keyed by the
    /// id [`Kernel::open_view`]/[`Kernel::open_window`] assigned that
    /// Window — the same id embedded in its page as
    /// `window.__rashomonWindowId`, and sent back as the `<window-id>:`
    /// prefix on every `window.cefQuery` request that Window's page
    /// sends. Several Window ids can map to the same View id; that's
    /// exactly what makes them mirrors of each other.
    windows: HashMap<String, String>,
    next_view_id: u64,
    next_window_id: u64,
}

/// Creates a Node exactly the way `rashomon::graph::store::Host::create_node`
/// does for Facets going through the WIT boundary — including the
/// link to this session's root thread (see
/// [`KernelState::session_root_id`]) — for the handful of Nodes
/// `run_browser_process`/[`record_access`] create directly in host
/// Rust code instead of through a Facet.
fn create_node_in_session(
    state: &mut KernelState,
    node_type: &str,
    role: rashomon_graph::Role,
    properties: HashMap<String, String>,
) -> rashomon_graph::Node {
    let node = state.graph.create_node(node_type, role, properties);
    if !state.session_root_id.is_empty() {
        state
            .graph
            .create_edge(PART_OF_THREAD_EDGE, &node.id, &state.session_root_id, 1.0);
    }
    node
}

/// Called from [`Kernel::open_view`] every time a View opens against
/// `node_id` — if that Node already existed *before* this call (it's
/// not an Entity, or it doesn't exist at all, this is a no-op) and
/// wasn't itself created earlier in *this* session (checked via
/// [`PART_OF_THREAD_EDGE`] rather than, say, a timestamp comparison,
/// since that edge is the one thing every session-created Node
/// unconditionally gets), this counts as freshly accessing something
/// that predates this session — recorded as a new Occurrence pointed
/// at it, the same `occurrence-of` relationship `terminal-xterm`'s own
/// `render()` creates by hand for a brand new shell session, just
/// driven here generically for *any* Entity instead of one Facet's
/// own domain-specific bookkeeping.
fn record_access(state: &mut KernelState, node_id: &str) {
    let Some(node) = state.graph.get_node(node_id) else { return };
    if !matches!(node.role, rashomon_graph::Role::Entity) {
        return;
    }
    let created_this_session = state
        .graph
        .query_edges_from(node_id)
        .into_iter()
        .any(|e| e.edge_type == PART_OF_THREAD_EDGE && e.target == state.session_root_id);
    if created_this_session {
        return;
    }
    let occurrence =
        create_node_in_session(state, "rashomon:view-access", rashomon_graph::Role::Occurrence, HashMap::new());
    state.graph.create_edge("occurrence-of", &occurrence.id, node_id, 1.0);
}

/// Called from `TabDisplayHandler::on_address_change` whenever a real
/// browser tab's main frame navigates somewhere that handler's own
/// per-tab dedup (see its doc comment) already decided is worth
/// recording. Finds or creates the one `rashomon:page` Entity for
/// `url` — the *first* navigation anywhere, ever, to a given URL is
/// what creates it (labeled with its own `url` property); every
/// navigation after that, in this session or a later one, finds it
/// here in [`KernelState::page_entities`] instead of creating a
/// duplicate, and gets recorded as a `rashomon:page-visit` Occurrence
/// of it instead.
fn record_page_visit(bridge: &Arc<Mutex<InputBridge>>, url: &str) {
    let mut bridge = bridge.lock().expect("input bridge lock poisoned");
    let state = bridge.store.data_mut();
    if let Some(entity_id) = state.page_entities.get(url).cloned() {
        let occurrence = create_node_in_session(
            state,
            "rashomon:page-visit",
            rashomon_graph::Role::Occurrence,
            HashMap::new(),
        );
        state.graph.create_edge("occurrence-of", &occurrence.id, &entity_id, 1.0);
    } else {
        let properties = HashMap::from([("url".to_string(), url.to_string())]);
        let entity = create_node_in_session(state, "rashomon:page", rashomon_graph::Role::Entity, properties);
        state.page_entities.insert(url.to_string(), entity.id);
    }
}

/// Everything needed to open a new View or mirror an existing one in a
/// new mounted tab: which Facets exist to instantiate, the live state
/// ([`InputBridge`]) either needs to register itself into, and the one
/// shared switcher ([`BrowserSwitcherState`]) every View's [`TabWidget`]
/// gets mounted into — the same one `create_tab` mounts browser tabs
/// into, so a terminal session and a browser tab are just two entries
/// in one list, switched between the same way.
struct Kernel {
    facets: FacetRegistry,
    bridge: Arc<Mutex<InputBridge>>,
    browser_switcher: Arc<Mutex<Option<BrowserSwitcherState>>>,
}

impl Kernel {
    /// Instantiates `facet_name`'s Component fresh, calls its `render`
    /// against `node_id` to get its first View, and mounts that View as
    /// a new tab in the one shared switcher window (see
    /// [`BrowserSwitcherState`]) — the same as
    /// [`Kernel::open_window`] would for any later tab mirroring this
    /// View, just with a fresh View instead of an existing one. Safe to
    /// call more than once against the same Facet: each call is an
    /// independent instantiation (independent `SESSION`-style guest
    /// state), the same way opening two terminal sessions in a real OS
    /// gives you two independent shells, not one shared one —
    /// [`Kernel::open_window`] is what shares one.
    fn open_view(&self, node_id: &str, facet_name: &str) -> Result<String> {
        let component = self.facets.component(facet_name)?;
        let mut bridge = self.bridge.lock().expect("input bridge lock poisoned");

        record_access(bridge.store.data_mut(), node_id);

        let bindings = FacetWorld::instantiate(&mut bridge.store, component, &self.facets.linker)
            .map_err(|e| anyhow!("failed to instantiate {facet_name}: {e}"))?;
        let initial_html = bindings
            .rashomon_facet_contract()
            .call_render(&mut bridge.store, node_id)
            .map_err(|e| anyhow!("{facet_name}'s render() failed: {e}"))?;

        let view_id = format!("view-{}", bridge.next_view_id);
        bridge.next_view_id += 1;
        bridge.views.insert(
            view_id.clone(),
            ViewHandle { bindings, initial_html, pending: HashMap::new() },
        );
        drop(bridge);

        self.open_window(&view_id)?;
        Ok(view_id)
    }

    /// Same instantiate-and-render step as [`Kernel::open_view`], but
    /// mounts into the one *fixed* sidebar slot (see
    /// [`Kernel::open_sidebar_view`]) instead of the switchable tab
    /// list — for the "sidebar" Facet itself, which isn't a tab to
    /// switch away from. Called once, from [`WinitKernelApp::resumed`],
    /// after the one native window exists for it to mount into.
    fn open_sidebar(&self, node_id: &str, facet_name: &str) -> Result<String> {
        let component = self.facets.component(facet_name)?;
        let mut bridge = self.bridge.lock().expect("input bridge lock poisoned");

        let bindings = FacetWorld::instantiate(&mut bridge.store, component, &self.facets.linker)
            .map_err(|e| anyhow!("failed to instantiate {facet_name}: {e}"))?;
        let initial_html = bindings
            .rashomon_facet_contract()
            .call_render(&mut bridge.store, node_id)
            .map_err(|e| anyhow!("{facet_name}'s render() failed: {e}"))?;

        let view_id = format!("view-{}", bridge.next_view_id);
        bridge.next_view_id += 1;
        bridge.views.insert(
            view_id.clone(),
            ViewHandle { bindings, initial_html, pending: HashMap::new() },
        );
        drop(bridge);

        self.open_sidebar_view(&view_id)?;
        Ok(view_id)
    }

    /// Registers a fresh window id for cefQuery routing against View
    /// `view_id`, then builds the actual [`TabWidget`] that renders
    /// it — the part [`Kernel::open_window`] (mounts into the
    /// switchable tab list) and [`Kernel::open_sidebar_view`] (mounts
    /// once, fixed) both need before deciding where the result goes.
    /// `rect` is the widget's initial bounds (mount sites decide their
    /// own — a tab gets the full content region, [`sidebar_rect`] is
    /// used for the sidebar). On macOS this is a transparent `wry`
    /// webview (see [`mac::FacetWebView`]) so it can actually show the
    /// background blur behind it — CEF's own `Browser` can never be
    /// made transparent in windowed mode (upstream issue
    /// chromiumembedded/cef#4035). Every other platform falls back to
    /// a real classic-embedded CEF `Browser` (opaque — `wry`/the
    /// background blur aren't wired up there yet).
    fn create_facet_widget(&self, view_id: &str, rect: Rect) -> Result<(String, TabWidget, Arc<Mutex<String>>)> {
        let mut bridge = self.bridge.lock().expect("input bridge lock poisoned");
        let view = bridge
            .views
            .get(view_id)
            .ok_or_else(|| anyhow!("no such view: {view_id}"))?;
        let html = view.initial_html.clone();

        let window_id = format!("window-{}", bridge.next_window_id);
        bridge.next_window_id += 1;
        bridge
            .views
            .get_mut(view_id)
            .expect("just looked this up above")
            .pending
            .insert(window_id.clone(), String::new());
        bridge.windows.insert(window_id.clone(), view_id.to_string());
        let bridge_handle = self.bridge.clone();
        // Dropped before calling into CEF/`wry`: both can turn around
        // and call back into a handler that also locks `self.bridge`
        // (CEF's `LifeSpanHandler::on_after_created`, `wry`'s own IPC
        // handler on the very first frame) before this function
        // returns, and `Mutex` isn't reentrant.
        drop(bridge);

        let title = Arc::new(Mutex::new(String::new()));

        #[cfg(target_os = "macos")]
        {
            let native_window_handle = self
                .browser_switcher
                .lock()
                .expect("browser switcher lock poisoned")
                .as_ref()
                .ok_or_else(|| anyhow!("no browser switcher window yet"))?
                .native_window_handle;
            let webview = mac::FacetWebView::new(
                &native_window_handle,
                &window_id,
                &html,
                cef_rect_to_wry(rect),
                title.clone(),
                bridge_handle,
            )?;
            Ok((window_id, TabWidget::Facet(webview), title))
        }

        #[cfg(not(target_os = "macos"))]
        {
            let mut client = ViewClient::new(bridge_handle, title.clone());
            let native_parent = self
                .browser_switcher
                .lock()
                .expect("browser switcher lock poisoned")
                .as_ref()
                .ok_or_else(|| anyhow!("no browser switcher window yet"))?
                .native_parent;
            let window_info = WindowInfo::default().set_as_child(native_parent, &rect);
            let settings = BrowserSettings::default();
            let url = CefString::from(html_data_uri(&inject_window_id(&html, &window_id)).as_str());
            let browser = browser_host_create_browser_sync(
                Some(&window_info),
                Some(&mut client),
                Some(&url),
                Some(&settings),
                None,
                None,
            )
            .ok_or_else(|| anyhow!("browser_host_create_browser_sync failed"))?;
            Ok((window_id, TabWidget::Browser(browser), title))
        }
    }

    /// Mounts a new tab into the shared switcher window mirroring the
    /// already-running View `view_id`. Input typed into this tab
    /// reaches the exact same Facet instance (and so the same PTY, for
    /// `terminal-xterm`) as every other tab mirroring this View; output
    /// is fanned out to all of them via `pending` (see [`ViewHandle`]).
    fn open_window(&self, view_id: &str) -> Result<String> {
        let rect = self
            .browser_switcher
            .lock()
            .expect("browser switcher lock poisoned")
            .as_ref()
            .map(|s| s.content_rect_full())
            .unwrap_or_else(|| content_rect_full(WINDOW_WIDTH_PX, WINDOW_HEIGHT_PX));
        let (window_id, widget, title) = self.create_facet_widget(view_id, rect)?;
        mount_tab(window_id.clone(), &self.browser_switcher, widget, title, TabKind::FacetView);
        Ok(window_id)
    }

    /// Mounts View `view_id` as the sidebar — not a switchable tab, so
    /// it doesn't go through [`mount_tab`] at all, just sits at
    /// [`BrowserSwitcherState::sidebar_rect`] for the lifetime of the
    /// window (kept up to date on resize — see
    /// [`BrowserSwitcherState::resize_window`]).
    fn open_sidebar_view(&self, view_id: &str) -> Result<String> {
        let rect = self
            .browser_switcher
            .lock()
            .expect("browser switcher lock poisoned")
            .as_ref()
            .map(|s| s.sidebar_rect())
            .unwrap_or_else(|| sidebar_rect(WINDOW_WIDTH_PX, WINDOW_HEIGHT_PX));
        let (window_id, widget, _title) = self.create_facet_widget(view_id, rect)?;

        let mut guard = self.browser_switcher.lock().expect("browser switcher lock poisoned");
        let switcher = guard.as_mut().ok_or_else(|| anyhow!("no browser switcher window yet"))?;
        switcher.sidebar_widget = Some(widget);

        Ok(window_id)
    }
}

/// `wry::Rect` uses the `dpi` crate's logical units; CEF's `Rect` is
/// plain integer pixels in the same window-relative coordinate space
/// `wry`'s `build_as_child`/`set_bounds` expect, so this is a straight
/// field-for-field conversion, not a real unit transform.
#[cfg(target_os = "macos")]
fn cef_rect_to_wry(rect: Rect) -> wry::Rect {
    wry::Rect {
        position: wry::dpi::LogicalPosition::new(rect.x as f64, rect.y as f64).into(),
        size: wry::dpi::LogicalSize::new(rect.width as f64, rect.height as f64).into(),
    }
}

/// Splices a `<script>` setting `window.__rashomonWindowId` right after
/// `html`'s `<head>` tag (or, failing that, right at the very start —
/// still valid, just less tidy) so a Facet's own page-template script
/// can read back which Window it's running as, without the Facet's
/// `render` (fixed by the WIT contract to take only a `node-id`)
/// needing to know about Windows at all — that's purely a host-side
/// concept.
fn inject_window_id(html: &str, window_id: &str) -> String {
    let script = format!("<script>window.__rashomonWindowId = {window_id:?};</script>");
    match html.find("<head>") {
        Some(idx) => {
            let insert_at = idx + "<head>".len();
            format!("{}{script}{}", &html[..insert_at], &html[insert_at..])
        }
        None => format!("{script}{html}"),
    }
}

/// Forces `html`/`body` transparent, `!important` so it wins regardless
/// of whatever opaque background a Facet's own CSS set (every current
/// Facet — `terminal-xterm`, `graph-view`, `extensions` — authors an
/// opaque one, since they were written before this host-level
/// requirement existed) — applied to every Facet View's page (not real
/// browser tabs, which stay fully opaque regardless) so the window's
/// background blur actually shows through wherever one is active, the
/// same way [`inject_window_id`] splices in host-owned behavior
/// without any Facet needing to know about it. Same insertion point as
/// `inject_window_id`, applied after it so both end up right after
/// `<head>`.
fn inject_transparent_background(html: &str) -> String {
    let style = "<style>html, body { background: transparent !important; }</style>";
    match html.find("<head>") {
        Some(idx) => {
            let insert_at = idx + "<head>".len();
            format!("{}{style}{}", &html[..insert_at], &html[insert_at..])
        }
        None => format!("{style}{html}"),
    }
}

/// The one router pair for this kernel's one window — see the module
/// doc comment for why a static is reasonable here (single browser
/// process, single window, for now).
static BROWSER_ROUTER: std::sync::OnceLock<Arc<BrowserSideRouter>> = std::sync::OnceLock::new();
static RENDERER_ROUTER: std::sync::OnceLock<Arc<RendererSideRouter>> = std::sync::OnceLock::new();

fn message_router_config() -> MessageRouterConfig {
    MessageRouterConfig::default()
}

struct InputQueryHandler {
    bridge: Arc<Mutex<InputBridge>>,
}

/// The magic suffix a Window's page sends instead of a real input event
/// to mean "poll for new output" — see
/// [`InputQueryHandler::on_query_str`]. A page polls itself (via
/// `setInterval` + `window.cefQuery`) rather than the host pushing into
/// it with `execute_java_script`, because
/// `execute_java_script` turns out to silently do nothing when called
/// from a CEF `Task` scheduled via `post_delayed_task` — only from a
/// genuine Client/Handler callback (confirmed empirically: identical
/// script, called from `on_load_end`, ran and logged; called from a
/// `Task::execute()`, returned normally but never actually ran). Polling
/// through `cefQuery`'s own request/response round trip sidesteps the
/// whole question, since that path is a proven-working Handler callback
/// the whole way, not a Task.
const POLL_REQUEST: &str = "__poll__";

/// Shared by every transport that can carry a Facet request —
/// currently CEF's `cefQuery` (`InputQueryHandler::on_query_str`) and
/// `wry`'s IPC handler (`FacetWebView`, used for the sidebar/urlbar/
/// non-browser Facets so they can live in a transparent native webview
/// instead of an opaque CEF `BrowserView` — see CEF issue #4035). Both
/// just parse the same `<window-id>:<event>` shape and land here, so
/// there's exactly one place that knows how to resolve a window id to
/// a View and call `handle-input`/`poll-output` on it. `event` is
/// either [`POLL_REQUEST`] (answered from `poll-output`, fanned out to
/// every Window mirroring this View — see [`ViewHandle`]) or real
/// input (answered from `handle-input`, which reaches the exact same
/// Facet instance no matter which mirroring Window sent it).
fn dispatch_facet_request(bridge: &Arc<Mutex<InputBridge>>, request: &str) -> Result<String, String> {
    let Some((window_id, event)) = request.split_once(':') else {
        return Err("malformed request: missing <window-id>: prefix".to_string());
    };

    let mut bridge = bridge.lock().expect("input bridge lock poisoned");
    let InputBridge { store, views, windows, .. } = &mut *bridge;
    let Some(view_id) = windows.get(window_id) else {
        return Err(format!("no such window: {window_id}"));
    };
    let Some(view) = views.get_mut(view_id) else {
        return Err(format!("no such view: {view_id}"));
    };

    if event == POLL_REQUEST {
        return view
            .bindings
            .rashomon_facet_contract()
            .call_poll_output(store)
            .map(|output| {
                if !output.is_empty() {
                    for pending in view.pending.values_mut() {
                        pending.push_str(&output);
                    }
                }
                view.pending.get_mut(window_id).map(std::mem::take).unwrap_or_default()
            })
            .map_err(|e| e.to_string());
    }

    view.bindings
        .rashomon_facet_contract()
        .call_handle_input(store, event)
        .map(|_| "ok".to_string())
        .map_err(|e| e.to_string())
}

impl BrowserSideHandler for InputQueryHandler {
    /// `request` is always `<window-id>:<event>` — every Window's page
    /// prefixes it that way before calling `window.cefQuery` (see
    /// `terminal-xterm`'s `render_page`) so this one handler, shared by
    /// every open Window, can dispatch to the right View via
    /// [`dispatch_facet_request`].
    fn on_query_str(
        &self,
        _browser: Option<Browser>,
        _frame: Option<Frame>,
        _query_id: i64,
        request: &str,
        _persistent: bool,
        callback: Arc<Mutex<dyn BrowserSideCallback>>,
    ) -> bool {
        let callback = callback.lock().expect("callback lock poisoned");
        match dispatch_facet_request(&self.bridge, request) {
            Ok(response) => callback.success_str(&response),
            Err(e) => callback.failure(-1, &e),
        }
        true
    }
}

// One fresh instance per View (see `Kernel::create_view_browser_view`),
// each with its own `title` `Arc` fed by `TabDisplayHandler` — the
// same title-tracking mechanism `TabClient` uses for real browser
// tabs, so `list-tabs` can report a real title uniformly regardless
// of whether the tab is a Facet View or a website.
wrap_client! {
    pub struct ViewClient {
        bridge: Arc<Mutex<InputBridge>>,
        title: Arc<Mutex<String>>,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(KernelLifeSpanHandler::new(self.bridge.clone()))
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(TitleOnlyDisplayHandler::new(self.title.clone()))
        }

        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> i32 {
            let Some(router) = BROWSER_ROUTER.get() else {
                return 0;
            };
            let handled = router.on_process_message_received(
                browser.cloned(),
                frame.cloned(),
                source_process,
                message.cloned(),
            );
            handled as i32
        }
    }
}

wrap_life_span_handler! {
    struct KernelLifeSpanHandler {
        bridge: Arc<Mutex<InputBridge>>,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, _browser: Option<&mut Browser>) {
            self.bridge.lock().expect("input bridge lock poisoned").browser_count += 1;
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            if let Some(router) = BROWSER_ROUTER.get() {
                router.on_before_close(browser.cloned());
            }
            self.bridge.lock().expect("input bridge lock poisoned").browser_count -= 1;
            // No more "last browser closes -> quit the app" behavior
            // (there used to be a `quit_message_loop()` call here): with
            // one persistent window hosting many tabs, closing one
            // tab/popup shouldn't close the whole app, and
            // `quit_message_loop()` itself only makes sense paired with
            // CEF's own `run_message_loop()`, which this no longer
            // uses (see `WinitKernelApp`/`external_message_pump`). The
            // window's own close button exits via `winit`'s
            // `WindowEvent::CloseRequested` instead.
        }
    }
}

/// The next time [`WinitKernelApp::about_to_wait`] should call
/// `do_message_loop_work()` — written by
/// `KernelBrowserProcessHandler::on_schedule_message_pump_work`
/// (CEF's documented hook for exactly this, under
/// `external_message_pump`), which can fire on any CEF thread, not
/// just `winit`'s — hence the `Mutex`, same reasoning as every other
/// cross-thread handoff in this module. Each call *replaces* whatever
/// was scheduled before, matching CEF's own contract for this
/// callback (it's the latest request that's authoritative, not an
/// additional one to merge in).
static NEXT_PUMP_DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);

/// Wakes `winit`'s event loop up from `ControlFlow::Wait`/`WaitUntil`
/// when `on_schedule_message_pump_work` schedules work sooner than
/// whatever `about_to_wait` was last told to sleep until — set once,
/// from `run_browser_process`, right after the `EventLoop` is created.
static PUMP_PROXY: std::sync::OnceLock<winit::event_loop::EventLoopProxy<()>> = std::sync::OnceLock::new();

/// Upper bound on how long [`WinitKernelApp::about_to_wait`] ever
/// sleeps before pumping CEF again, regardless of whether
/// `on_schedule_message_pump_work` asked for anything sooner — a
/// guaranteed floor, not the primary scheduling signal (that's still
/// `NEXT_PUMP_DEADLINE`, which wins whenever it's sooner). Comfortably
/// sub-frame (60fps ≈ 16.7ms) so the browser compositor is never
/// starved for noticeably long, while still nowhere near as hot as
/// pumping on literally every `ControlFlow::Poll` tick (which could
/// run at any multiple of that rate the OS allows).
const PUMP_FALLBACK_INTERVAL: Duration = Duration::from_millis(16);

// `winit` drives the blocking run loop now (see `WinitKernelApp`), so
// CEF is configured with `external_message_pump: 1` — this handler's
// job is purely to record *when* CEF next wants `do_message_loop_work()`
// called again (see `NEXT_PUMP_DEADLINE`) and make sure `winit`'s loop
// is actually awake to notice, rather than `about_to_wait` pumping
// unconditionally on every tick under `ControlFlow::Poll` the way it
// used to — that kept CEF correctly fed, but also pinned a full CPU
// core busy-looping even with the app sitting completely idle. Window/
// tab/sidebar setup (previously done here, in `on_context_initialized`,
// since that used to be the earliest hook with a CEF context ready)
// has moved to `WinitKernelApp::resumed`, since it needs the native
// window `winit` creates, which doesn't exist yet by the time
// `on_context_initialized` fires.
wrap_browser_process_handler! {
    struct KernelBrowserProcessHandler {}

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            let deadline = Instant::now() + Duration::from_millis(delay_ms.max(0) as u64);
            *NEXT_PUMP_DEADLINE.lock().expect("pump deadline lock poisoned") = Some(deadline);
            if let Some(proxy) = PUMP_PROXY.get() {
                // Only actually needed when the loop is currently
                // asleep past this new, sooner deadline — but harmless
                // (just one extra wake-and-recompute) to send
                // unconditionally rather than tracking that.
                let _ = proxy.send_event(());
            }
        }
    }
}

wrap_render_process_handler! {
    struct KernelRenderProcessHandler {}

    impl RenderProcessHandler {
        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            let router = RENDERER_ROUTER.get_or_init(|| RendererSideRouter::new(message_router_config()));
            router.on_context_created(browser.cloned(), frame.cloned(), context.cloned());
        }

        fn on_context_released(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            if let Some(router) = RENDERER_ROUTER.get() {
                router.on_context_released(browser.cloned(), frame.cloned(), context.cloned());
            }
        }

        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> i32 {
            let Some(router) = RENDERER_ROUTER.get() else {
                return 0;
            };
            let handled = router.on_process_message_received(
                browser.cloned(),
                frame.cloned(),
                Some(source_process),
                message.cloned(),
            );
            handled as i32
        }
    }
}

wrap_app! {
    pub struct KernelApp {
        extension_configs: Arc<Vec<BrowserExtensionConfig>>,
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(KernelBrowserProcessHandler::new())
        }

        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(KernelRenderProcessHandler::new())
        }

        /// Only the browser process (empty/absent `--type`) owns
        /// extension loading — subprocesses re-parse the same command
        /// line but don't need these switches appended a second time.
        /// Settled by hands-on CEF spiking in `cef-extension-spike`.
        fn on_before_command_line_processing(
            &self,
            process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            if process_type.is_some() || self.extension_configs.is_empty() {
                return;
            }
            let Some(command_line) = command_line else { return };
            let paths = self
                .extension_configs
                .iter()
                .map(|ext| ext.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(",");
            command_line.append_switch_with_value(
                Some(&CefString::from("load-extension")),
                Some(&CefString::from(paths.as_str())),
            );
            command_line.append_switch_with_value(
                Some(&CefString::from("disable-extensions-except")),
                Some(&CefString::from(paths.as_str())),
            );
        }
    }
}

/// `winit`'s `ApplicationHandler` — the one blocking run loop this
/// process actually uses (see the module doc comment); CEF is pumped
/// from inside [`WinitKernelApp::about_to_wait`] instead of owning its
/// own. All of the windowing setup that used to live in
/// `KernelBrowserProcessHandler::on_context_initialized` moved to
/// [`WinitKernelApp::resumed`] instead, since it needs the native
/// window `winit` creates — which, unlike CEF's own Views `Window`,
/// doesn't exist yet by the time CEF's context finishes initializing.
struct WinitKernelApp {
    kernel: Arc<Kernel>,
    initial_views: Arc<Vec<(String, String, u32)>>,
    browser_switcher: Arc<Mutex<Option<BrowserSwitcherState>>>,
    browser_component: Arc<Component>,
    browser_demo_node_id: String,
    window: Option<Window>,
}

impl WinitKernelApp {
    /// Re-derives the window's raw `NSView*` and calls
    /// [`mac::apply_background_blur`] again — see
    /// [`WinitKernelApp::window_event`]'s doc comment for why this
    /// needs to happen more than once.
    #[cfg(target_os = "macos")]
    fn reapply_background_blur(&self) {
        let Some(window) = &self.window else { return };
        let Ok(handle) = window.window_handle() else { return };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return };
        mac::apply_background_blur(handle.ns_view.as_ptr().cast(), 40);
    }

    /// Relayouts the sidebar/urlbar/active tab against `size` — see
    /// [`BrowserSwitcherState::resize_window`]. `size` is converted to
    /// logical units (the same ones `WINDOW_WIDTH_PX`/`WINDOW_HEIGHT_PX`
    /// and every native frame/`Rect` in this module are already in)
    /// before use, since `WindowEvent::Resized` itself reports physical
    /// pixels — on a Retina/HiDPI display those differ from logical
    /// units by the window's scale factor.
    fn resize_switcher(&self, size: winit::dpi::PhysicalSize<u32>) {
        let Some(window) = &self.window else { return };
        let logical: winit::dpi::LogicalSize<f64> = size.to_logical(window.scale_factor());
        let mut guard = self.browser_switcher.lock().expect("browser switcher lock poisoned");
        if let Some(switcher) = guard.as_mut() {
            switcher.resize_window(logical.width.round() as i32, logical.height.round() as i32);
        }
    }
}

impl ApplicationHandler for WinitKernelApp {
    /// Fires once, before the event loop starts ticking — builds the
    /// one native window every tab/the sidebar/the urlbar attach to,
    /// then does everything `on_context_initialized` used to:
    /// registers the message-router handler, renders the `browser`
    /// Facet's demo `create-tab` calls, opens every startup View (and
    /// its mirror Windows), and opens the sidebar last (after real
    /// tab content already exists — matters for the sidebar Facet's
    /// own `list-tabs` polling to see something non-empty right away).
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let mut attrs = Window::default_attributes()
            .with_title("Rashomon")
            .with_inner_size(winit::dpi::LogicalSize::new(WINDOW_WIDTH_PX as f64, WINDOW_HEIGHT_PX as f64))
            // Resizable — `WindowEvent::Resized` (see `window_event`)
            // keeps the sidebar/urlbar/active tab's bounds in sync
            // with the window's actual current size (see
            // `BrowserSwitcherState::resize_window`).
            // `winit` windows are opaque by default — needed in
            // addition to `wry`'s own per-webview transparency so the
            // background blur is actually visible anywhere a
            // transparent Facet/sidebar/urlbar webview renders.
            .with_transparent(true);
        #[cfg(target_os = "macos")]
        {
            attrs = attrs
                .with_titlebar_transparent(true)
                .with_title_hidden(true)
                .with_fullsize_content_view(true)
                .with_movable_by_window_background(true);
        }
        let window = event_loop.create_window(attrs).expect("create_window failed");

        let raw_handle = window.window_handle().expect("window_handle failed").as_raw();

        #[cfg(target_os = "macos")]
        let native_window_handle = {
            let RawWindowHandle::AppKit(handle) = raw_handle else {
                panic!("expected an AppKit window handle on macOS");
            };
            let ptr: *mut std::ffi::c_void = handle.ns_view.as_ptr().cast();
            mac::apply_background_blur(ptr, 40);
            mac::NativeWindowHandle::from_ns_view_ptr(ptr)
                .expect("window_handle should be realized right after create_window")
        };

        let native_parent: cef::sys::cef_window_handle_t = match raw_handle {
            #[cfg(target_os = "macos")]
            RawWindowHandle::AppKit(handle) => handle.ns_view.as_ptr().cast(),
            #[cfg(target_os = "windows")]
            RawWindowHandle::Win32(handle) => handle.hwnd.get() as cef::sys::cef_window_handle_t,
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xlib(handle) => handle.window as cef::sys::cef_window_handle_t,
            _ => panic!("unsupported platform/window handle kind for this architecture"),
        };

        #[cfg(target_os = "macos")]
        let urlbar = {
            let browser_switcher = self.browser_switcher.clone();
            mac::UrlBarWebView::new(&native_window_handle, cef_rect_to_wry(urlbar_rect(WINDOW_WIDTH_PX)), move |text| {
                let url = normalize_url_input(&text);
                if let Some(switcher) = browser_switcher.lock().expect("browser switcher lock poisoned").as_ref() {
                    switcher.navigate_active(&url);
                }
            })
            .expect("urlbar webview creation failed")
        };

        *self.browser_switcher.lock().expect("browser switcher lock poisoned") = Some(BrowserSwitcherState {
            #[cfg(target_os = "macos")]
            native_window_handle,
            native_parent,
            #[cfg(target_os = "macos")]
            urlbar,
            tabs: Vec::new(),
            active_index: 0,
            sidebar_widget: None,
            window_width: WINDOW_WIDTH_PX,
            window_height: WINDOW_HEIGHT_PX,
        });

        let router = BROWSER_ROUTER.get_or_init(|| BrowserSideRouter::new(message_router_config()));
        router.add_handler(Arc::new(InputQueryHandler { bridge: self.kernel.bridge.clone() }), false);

        {
            let mut bridge = self.kernel.bridge.lock().expect("input bridge lock poisoned");
            let result = FacetWorld::instantiate(&mut bridge.store, &self.browser_component, &self.kernel.facets.linker)
                .map_err(|e| anyhow!("failed to instantiate browser component: {e}"))
                .and_then(|bindings| {
                    bindings
                        .rashomon_facet_contract()
                        .call_render(&mut bridge.store, &self.browser_demo_node_id)
                        .map_err(|e| anyhow!("browser component's render() failed: {e}"))
                });
            drop(bridge);
            match result {
                Ok(summary) => println!("browser component rendered: {summary}"),
                Err(e) => eprintln!("{e}"),
            }
        }

        for (node_id, facet_name, window_count) in self.initial_views.iter() {
            let view_id = match self.kernel.open_view(node_id, facet_name) {
                Ok(view_id) => view_id,
                Err(e) => {
                    eprintln!("failed to open initial view for {facet_name}: {e}");
                    continue;
                }
            };
            for _ in 1..*window_count {
                if let Err(e) = self.kernel.open_window(&view_id) {
                    eprintln!("failed to open mirror window for {facet_name}: {e}");
                }
            }
        }

        // The sidebar is a real Facet Component (`components/sidebar`),
        // not inline host code. Opened last, after real tab content
        // already exists, so the sidebar Facet's own `list-tabs`
        // polling has something non-empty to show immediately.
        if let Err(e) = self.kernel.open_sidebar("sidebar-view", "sidebar") {
            eprintln!("failed to open sidebar: {e}");
        }

        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _window_id: WindowId, event: WindowEvent) {
        // The private CGS blur call needs reapplying after the window
        // server's own compositing state for this window changes —
        // confirmed empirically: it's visible right after creation,
        // then silently drops out the next time the window is
        // resized, moved, or regains key status (e.g. after clicking
        // back into it from another app). Simplest fix found was to
        // just reapply it on exactly those `winit` events, rather than
        // depend on a single call at startup surviving every future
        // compositing change.
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.resize_switcher(size);
                #[cfg(target_os = "macos")]
                self.reapply_background_blur();
            }
            #[cfg(target_os = "macos")]
            WindowEvent::Moved(_) | WindowEvent::Focused(true) => {
                self.reapply_background_blur();
            }
            _ => {}
        }
    }

    /// `external_message_pump` means CEF never gets a blocking run
    /// loop of its own — it instead calls back into
    /// `KernelBrowserProcessHandler::on_schedule_message_pump_work`
    /// (from *any* thread) whenever it wants `do_message_loop_work()`
    /// called again, recording that request in `NEXT_PUMP_DEADLINE`
    /// and waking this loop via `PUMP_PROXY` if it's currently asleep.
    ///
    /// Pumps unconditionally on *every* wakeup (not just when
    /// `NEXT_PUMP_DEADLINE` says it's due) — purely relying on that
    /// callback regressed to every browser rendering solid white,
    /// confirmed empirically: there's no prior evidence in this
    /// project that CEF calls it densely enough on its own to keep a
    /// real compositor fed (the validated `cef-winit-spike` this
    /// architecture is ported from never actually depended on it
    /// either — it only ever pumped unconditionally under
    /// `ControlFlow::Poll`). `NEXT_PUMP_DEADLINE`/`PUMP_PROXY` are kept
    /// as a *responsiveness* optimization — waking early for input
    /// CEF flagged as urgent — layered on top of a guaranteed
    /// `PUMP_FALLBACK_INTERVAL` floor, not as the sole trigger.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        do_message_loop_work();
        let now = Instant::now();
        let requested = NEXT_PUMP_DEADLINE.lock().expect("pump deadline lock poisoned").take();
        let fallback = now + PUMP_FALLBACK_INTERVAL;
        let next_wake = match requested {
            Some(when) => when.min(fallback),
            None => fallback,
        };
        event_loop.set_control_flow(ControlFlow::WaitUntil(next_wake));
    }
}

/// An `App` with no `Kernel` yet — used for the initial `execute_process`
/// dispatch by *both* binaries, before we know whether this invocation is
/// the browser process (which gets a real `KernelApp` later, via
/// [`run_browser_process`]) or a subprocess (for which this is the only
/// `App` it will ever get, and the only thing that matters is that
/// `render_process_handler()` still works without a `Kernel`, which it
/// does).
pub fn make_minimal_app() -> App {
    KernelApp::new(Arc::new(Vec::new()))
}

/// Everything that happens once we know this process is the CEF browser
/// process: run the one-shot `ping`/`shell`/`terminal` console demos
/// (unchanged — console-only, never opened as Views), build a [`Kernel`]
/// that knows how to open `terminal-xterm` Views, open one of them
/// mirrored into two Windows (one `bash` session, two Windows watching
/// it — see [`Kernel::open_window`]) to prove that side of the design,
/// then hand off to CEF's own run loop. Blocks until every Window is
/// closed.
pub fn run_browser_process(args: &args::Args) -> Result<()> {
    let ping_wasm_path = ensure_component_built("ping")?;
    let shell_wasm_path = ensure_component_built("shell")?;
    let terminal_wasm_path = ensure_component_built("terminal")?;
    let terminal_xterm_wasm_path = ensure_component_built("terminal-xterm")?;
    let browser_wasm_path = ensure_component_built("browser")?;
    let sidebar_wasm_path = ensure_component_built("sidebar")?;
    let graph_view_wasm_path = ensure_component_built("graph-view")?;
    let extensions_wasm_path = ensure_component_built("extensions")?;
    let graph_db_path = graph_db_path()?;

    let engine = Engine::default();
    let mut linker: Linker<KernelState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    FacetWorld::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;

    println!("graph database: {}", graph_db_path.display());

    let browser_switcher: Arc<Mutex<Option<BrowserSwitcherState>>> = Arc::new(Mutex::new(None));

    // Loaded from the persisted config file, not hardcoded — see
    // `BrowserExtensionConfig`'s doc comment and the "extensions"
    // Facet's add/remove buttons, which are the normal way this file
    // gets populated. Empty (not an error) on a fresh install.
    let all_configured_extensions = load_configured_extensions().unwrap_or_else(|e| {
        eprintln!("failed to load extension config: {e}");
        Vec::new()
    });
    let (valid_extensions, invalid_extensions): (Vec<_>, Vec<_>) =
        all_configured_extensions.into_iter().partition(configured_extension_is_valid);
    for ext in &invalid_extensions {
        eprintln!(
            "extension {:?} at {} is no longer valid (not Manifest V3) — removing from config \
             automatically rather than handing it to --load-extension and having CEF reject it \
             on every startup",
            ext.name,
            ext.path.display()
        );
    }
    if !invalid_extensions.is_empty() {
        if let Err(e) = save_configured_extensions(&valid_extensions) {
            eprintln!("failed to save cleaned-up extension config: {e}");
        }
    }
    let extension_configs: Arc<Vec<BrowserExtensionConfig>> = Arc::new(valid_extensions);
    let extension_candidates: Arc<Vec<ExtensionCandidate>> = Arc::new(discover_extension_candidates());
    let extensions: Arc<Vec<ExtensionRuntime>> = Arc::new(
        extension_configs
            .iter()
            .cloned()
            .map(|config| ExtensionRuntime { config, popup: Arc::new(Mutex::new(PopupState::Closed)) })
            .collect(),
    );

    let mut store = Store::new(
        &engine,
        KernelState {
            graph: rashomon_graph::PersistentGraphStore::open(&graph_db_path),
            wasi_ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            extensions: extensions.clone(),
            extension_candidates: extension_candidates.clone(),
            browser_switcher: browser_switcher.clone(),
            // Set for real immediately below, once there's a `graph`
            // to create it against — empty here only because
            // something has to be here first (`create_node_in_session`
            // treats an empty id as "no session root yet" and skips
            // linking, which is exactly right for the one Node, this
            // one, that must not end up linked to itself).
            session_root_id: String::new(),
            // Populated for real immediately below, from whatever
            // `rashomon:page` Entities already exist — empty here
            // only because the graph hasn't been scanned yet at this
            // exact point.
            page_entities: HashMap::new(),
            // Set for real immediately below, once the `Arc` wrapping
            // this very `Store` actually exists to clone — see
            // `KernelState::bridge_handle`'s doc comment for why this
            // two-step, set-it-right-after-construction dance is
            // needed at all.
            bridge_handle: None,
        },
    );

    // This session's one root `rashomon:thread` — every other Node any
    // Facet creates from here on (via `create-node`, or the handful
    // `run_browser_process` itself still creates directly below) gets
    // linked to it automatically (see `create_node_in_session`), and
    // it's the fixed point `record_access` checks a Node against to
    // tell "created this session" apart from "existed before it."
    let session_started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
        .to_string();
    let session_root_id = store
        .data_mut()
        .graph
        .create_node(
            "rashomon:thread",
            rashomon_graph::Role::Entity,
            HashMap::from([("started-at".to_string(), session_started_at)]),
        )
        .id;
    store.data_mut().session_root_id = session_root_id.clone();
    println!("session root thread: {session_root_id}");

    // Replays every `rashomon:page` Entity already persisted (from
    // this Node's own earlier sessions) into `page_entities`, so a
    // browser tab navigating to a URL it's seen before — even in a
    // completely different run of the app — finds the existing Entity
    // instead of minting a duplicate. See `record_page_visit`.
    let page_entities: HashMap<String, String> = store
        .data()
        .graph
        .all_nodes()
        .into_iter()
        .filter(|node| node.node_type == "rashomon:page" && matches!(node.role, rashomon_graph::Role::Entity))
        .filter_map(|node| node.properties.get("url").cloned().map(|url| (url, node.id)))
        .collect();
    store.data_mut().page_entities = page_entities;

    // `wasmtime::Error` doesn't implement `std::error::Error`, so
    // anyhow's `.context()`/`.with_context()` extension methods don't
    // apply directly to a `wasmtime::Result` — map it by hand throughout.
    let ping_component = Component::from_file(&engine, &ping_wasm_path)
        .map_err(|e| anyhow!("failed to load component at {}: {e}", ping_wasm_path.display()))?;
    let ping_bindings = FacetWorld::instantiate(&mut store, &ping_component, &linker)
        .map_err(|e| anyhow!("failed to instantiate ping component: {e}"))?;
    let rendered = ping_bindings
        .rashomon_facet_contract()
        .call_render(&mut store, "demo-node")
        .map_err(|e| anyhow!("call to ping's render() failed: {e}"))?;
    println!("ping component rendered: {rendered}");

    // A durable Entity for the shell session's `occurrence-of` edge to
    // attach to — a generic `thread`, per the doc's own fallback ("the
    // default place to file anything with no more specific type").
    // Created directly against the graph rather than through a
    // Component, since nothing installs/creates Threads yet.
    let thread_id = create_node_in_session(
        store.data_mut(),
        "rashomon:thread",
        rashomon_graph::Role::Entity,
        Default::default(),
    )
    .id;
    println!("created thread entity: {thread_id}");

    let shell_component = Component::from_file(&engine, &shell_wasm_path)
        .map_err(|e| anyhow!("failed to load component at {}: {e}", shell_wasm_path.display()))?;
    let shell_bindings = FacetWorld::instantiate(&mut store, &shell_component, &linker)
        .map_err(|e| anyhow!("failed to instantiate shell component: {e}"))?;
    let rendered_shell = shell_bindings
        .rashomon_facet_contract()
        .call_render(&mut store, &thread_id)
        .map_err(|e| anyhow!("call to shell's render() failed: {e}"))?;
    println!("shell component rendered: {rendered_shell}");

    // Prove the write actually landed in the persistent graph, not just
    // in the Component's return value.
    let occurrences = store.data().graph.query_edges_to(&thread_id);
    println!("occurrence-of edges now pointing at the thread: {occurrences:?}");

    let terminal_component = Component::from_file(&engine, &terminal_wasm_path).map_err(|e| {
        anyhow!("failed to load component at {}: {e}", terminal_wasm_path.display())
    })?;
    let terminal_bindings = FacetWorld::instantiate(&mut store, &terminal_component, &linker)
        .map_err(|e| anyhow!("failed to instantiate terminal component: {e}"))?;
    let initial_screen = terminal_bindings
        .rashomon_facet_contract()
        .call_render(&mut store, &thread_id)
        .map_err(|e| anyhow!("call to terminal's render() failed: {e}"))?;
    println!("terminal component rendered: {initial_screen:?}");

    // One Entity for the `terminal-xterm` View below — a single shell
    // session, mirrored into two Windows (see `initial_views`), so it
    // gets one `occurrence-of` target, not two.
    let mirrored_thread = create_node_in_session(
        store.data_mut(),
        "rashomon:thread",
        rashomon_graph::Role::Entity,
        Default::default(),
    )
    .id;

    // A durable Entity for the browser Facet's `create-tab` calls to
    // attach their `rashomon:page` Occurrences to, same reasoning as
    // `thread_id` above.
    let browser_demo_node_id = create_node_in_session(
        store.data_mut(),
        "rashomon:thread",
        rashomon_graph::Role::Entity,
        Default::default(),
    )
    .id;

    let terminal_xterm_component =
        Component::from_file(&engine, &terminal_xterm_wasm_path).map_err(|e| {
            anyhow!(
                "failed to load component at {}: {e}",
                terminal_xterm_wasm_path.display()
            )
        })?;

    // Not opened via `Kernel::open_view` (which assumes a Facet's View
    // is HTML rendered into a `data:` URI window) — its `render()` is
    // called directly in `on_context_initialized`, once the native
    // browser switcher window exists for its `create-tab` calls to
    // mount into. See `KernelBrowserProcessHandler`.
    let browser_component = Component::from_file(&engine, &browser_wasm_path)
        .map_err(|e| anyhow!("failed to load component at {}: {e}", browser_wasm_path.display()))?;

    // Unlike `browser_component` above, this one *does* go through
    // `Kernel::open_sidebar` (which looks it up via
    // `FacetRegistry::component`, same as any other registered Facet)
    // — the sidebar is a regular View as far as instantiation goes,
    // just mounted into a fixed slot instead of the switchable one.
    let sidebar_component = Component::from_file(&engine, &sidebar_wasm_path)
        .map_err(|e| anyhow!("failed to load component at {}: {e}", sidebar_wasm_path.display()))?;

    // Unlike `sidebar`, this one needs no special mounting target at
    // all — it's just another tab in the switcher, opened via
    // `Kernel::open_view` exactly like `terminal-xterm` (see
    // `initial_views` below). It doesn't write to the graph itself, so
    // the `node_id` it's rendered against doesn't need to mean
    // anything in particular.
    let graph_view_component = Component::from_file(&engine, &graph_view_wasm_path)
        .map_err(|e| anyhow!("failed to load component at {}: {e}", graph_view_wasm_path.display()))?;

    // Same as `graph-view`: just another tab, `node_id` unused.
    let extensions_component = Component::from_file(&engine, &extensions_wasm_path)
        .map_err(|e| anyhow!("failed to load component at {}: {e}", extensions_wasm_path.display()))?;

    let mut facets = HashMap::new();
    facets.insert("terminal-xterm".to_string(), terminal_xterm_component);
    facets.insert("sidebar".to_string(), sidebar_component);
    facets.insert("graph-view".to_string(), graph_view_component);
    facets.insert("extensions".to_string(), extensions_component);

    // `store` moves into the bridge here — every further call into any
    // Component's `render`/`handle-input`/`poll-output` goes through
    // `Kernel::open_view` or `InputQueryHandler` from now on, not
    // through any further direct use in this function.
    let bridge = Arc::new(Mutex::new(InputBridge {
        store,
        browser_count: 0,
        views: HashMap::new(),
        windows: HashMap::new(),
        next_view_id: 0,
        next_window_id: 0,
    }));
    // See `KernelState::bridge_handle`'s doc comment for why
    // `create_tab` needs this clone of the very `Arc` that wraps the
    // `Store` its own `KernelState` lives inside.
    bridge.lock().expect("input bridge lock poisoned").store.data_mut().bridge_handle = Some(bridge.clone());
    let kernel = Arc::new(Kernel {
        facets: FacetRegistry { linker, components: facets },
        bridge,
        browser_switcher: browser_switcher.clone(),
    });

    // One `terminal-xterm` View, mirrored into two Windows: typing in
    // either reaches the same PTY, and output (including the PTY's own
    // echo of that input) is fanned out to both — see `ViewHandle` and
    // `Kernel::open_window`. `graph-view` reuses `thread_id` (already
    // created above) purely because `render()` needs *some* node id —
    // it shows the whole graph, not `thread_id`'s neighborhood.
    let initial_views = Arc::new(vec![
        (mirrored_thread, "terminal-xterm".to_string(), 2u32),
        (thread_id.clone(), "graph-view".to_string(), 1u32),
        (thread_id, "extensions".to_string(), 1u32),
    ]);

    let mut app = KernelApp::new(extension_configs);
    let settings = Settings {
        no_sandbox: 1,
        // `winit` owns the blocking run loop (see `WinitKernelApp`
        // below) — CEF is pumped from inside it instead of owning its
        // own via `run_message_loop()`, the same `external_message_pump`
        // integration `crates/cef-winit-spike` validated.
        external_message_pump: 1,
        ..Default::default()
    };
    ensure!(
        initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) == 1,
        "cef initialize() failed"
    );

    #[cfg(target_os = "macos")]
    let _delegate = mac::setup_kernel_app_delegate();

    let event_loop = EventLoop::new().context("winit EventLoop::new failed")?;
    // Lets `KernelBrowserProcessHandler::on_schedule_message_pump_work`
    // (called from any CEF thread) wake this loop up on demand instead
    // of it having to busy-poll to notice new scheduled work — see
    // `NEXT_PUMP_DEADLINE`/`about_to_wait`.
    let _ = PUMP_PROXY.set(event_loop.create_proxy());
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut winit_app = WinitKernelApp {
        kernel,
        initial_views,
        browser_switcher,
        browser_component: Arc::new(browser_component),
        browser_demo_node_id,
        window: None,
    };
    event_loop.run_app(&mut winit_app).context("winit run_app failed")?;

    shutdown();

    Ok(())
}
