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
//! [`open_browser_switcher_window`]/[`BrowserSwitcherState`]), all
//! sharing one `Client`/`InputQueryHandler`/`Store` via [`InputBridge`].
//!
//! **Why one window, with real native panes, not an iframe-pane HTML
//! shell:** the first attempt loaded a single shell page and inserted
//! each View as an `<iframe srcdoc>` pane via `execute_java_script`.
//! Two separate bugs showed up chasing that down, both confirmed
//! empirically rather than assumed: (1) `execute_java_script` silently
//! does nothing when called from a CEF `Task` (e.g. one scheduled via
//! `post_delayed_task`) — identical script, called from a genuine
//! Client/Handler callback like `on_load_end`, ran and logged; called
//! from `Task::execute()`, returned normally but never actually ran.
//! That one's dodged below by routing output through the same
//! `cefQuery` round trip input already uses ([`POLL_REQUEST`]) instead
//! of a host-side push loop. (2) More fundamentally, dynamically-created
//! `<iframe srcdoc>` elements never finished navigating in this
//! CEF/Alloy configuration at all. Real multi-pane support turned out to
//! belong to native child-view embedding instead of HTML iframes — each
//! View/tab is a real Views-framework `BrowserView` (see `create_tab`,
//! `Kernel::open_window`), mounted into one shared, swappable panel
//! (`BrowserSwitcherState::active_region`) exactly the way
//! `crates/cef-extension-spike` validated it, rather than embedded via
//! HTML at all.
//!
//! Input flows back via a CEF message-router bridge: each tab's page
//! prefixes its `window.cefQuery` calls with its own window id, so one
//! shared [`InputQueryHandler`] can route a keystroke (or a
//! [`POLL_REQUEST`]) to the right View's `handle-input` (or
//! `poll-output`) rather than there being one handler per tab. More
//! than one tab can mirror the same View this way — see
//! [`Kernel::open_window`] — with output fanned out so no mirroring tab
//! loses it to whichever one happens to poll first. Every
//! primitive is backed by a real host
//! implementation — `rashomon:graph` by a `PersistentGraphStore` (so
//! the graph survives a restart), and `rashomon:process` by a real PTY
//! (`portable-pty`), not plain OS pipes — `resize`/`signal` are genuine
//! now. Windowing is CEF (validated in `crates/cef-spike`), not
//! `winit`/`softbuffer` — CEF owns the main run loop, so the two can't
//! coexist in one process anyway.
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

use anyhow::{anyhow, bail, ensure, Context, Result};
use cef::wrapper::message_router::*;
use cef::*;
use directories::ProjectDirs;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use rashomon_graph::GraphStore;
use serde::{Deserialize, Serialize};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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
    /// `None` until [`open_browser_switcher_window`] builds the one
    /// native window every tab gets mounted into — `create_tab` must
    /// never be called before that happens (see
    /// `KernelBrowserProcessHandler::on_context_initialized`).
    browser_switcher: Arc<Mutex<Option<BrowserSwitcherState>>>,
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

impl rashomon::graph::store::Host for KernelState {
    fn create_node(
        &mut self,
        node_type: String,
        node_role: rashomon::graph::types::Role,
        properties: Vec<rashomon::graph::types::Property>,
    ) -> rashomon::graph::types::Node {
        let properties = properties.into_iter().map(|p| (p.key, p.value)).collect();
        let node = self
            .graph
            .create_node(&node_type, to_lib_role(node_role), properties);
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

/// Shared by `HostBrowserContext::toggle_extension_popup` (the WIT-facing
/// path, called from a Facet Component) and `SwitcherOpenPopupButtonDelegate`
/// (the native "Open Popup" button on the browser switcher window built
/// by [`open_browser_switcher_window`]) — both reproduce the exact
/// open/close-race-safe toggle `cef-extension-spike` validated, via this
/// one function, rather than two parallel copies of the same logic.
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

/// A real `RuntimeStyle::ALLOY` `BrowserView` — never in its own
/// dedicated window. `create_tab` mounts every tab's `BrowserView` into
/// the one shared, swappable active-region panel
/// [`open_browser_switcher_window`] builds (Alloy-style BrowserViews,
/// unlike Chrome-style ones, have no "only one per window" restriction
/// — confirmed in `cef-extension-spike`), the same one-window design
/// that spike's tab-switching + popup-toggle behavior validated.
pub struct BrowserTab {
    id: String,
    browser_view: BrowserView,
    title: Arc<Mutex<String>>,
}

/// One tab mounted or mountable into [`BrowserSwitcherState::active_region`]
/// — a cheap `BrowserView` clone (Views-framework handles are cheap to
/// duplicate; CEF's own `BrowserHost`/`Browser` stays singular either
/// way). This doesn't fight the WIT `browser-tab` resource's single-owner
/// rule since it's pure host-side bookkeeping, entirely independent of
/// whichever Facet Component's [`BrowserTab`] handle `create-tab` also
/// returned for the same underlying browser.
/// `root_panel`/`root_layout` are here (not just `active_region`) so
/// the sidebar — now a real Facet View, not inline host code, see
/// [`Kernel::open_sidebar_view`] — can be mounted into the same
/// horizontal split *after* [`open_browser_switcher_window`] returns,
/// once the "sidebar" Facet has actually been instantiated and
/// rendered.
struct BrowserSwitcherState {
    root_panel: Panel,
    root_layout: Option<BoxLayout>,
    active_region: Panel,
    /// `(tab-id, view)` — `tab-id` is whatever `create_tab`/`open_window`
    /// already generates (a `BrowserTab`'s own id, or a Facet View's
    /// window id), exposed to guests via `list-tabs`/`switch-to-tab` so
    /// a sidebar Facet can address a tab without ever holding (or being
    /// handed) its `browser-tab` resource.
    tabs: Vec<(String, BrowserView)>,
    active_index: usize,
}

impl BrowserSwitcherState {
    fn switch_to(&mut self, index: usize) {
        if index == self.active_index || index >= self.tabs.len() {
            return;
        }
        let mut current = View::from(&self.tabs[self.active_index].1);
        self.active_region.remove_child_view(Some(&mut current));
        let mut next = View::from(&self.tabs[index].1);
        self.active_region.add_child_view(Some(&mut next));
        self.active_region.layout();
        self.active_index = index;
    }

    fn switch_to_id(&mut self, tab_id: &str) -> Result<(), String> {
        let index = self
            .tabs
            .iter()
            .position(|(id, _)| id == tab_id)
            .ok_or_else(|| format!("no such tab: {tab_id}"))?;
        self.switch_to(index);
        Ok(())
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

/// Registers `browser_view` as a new tab in the shared switcher under
/// `tab_id`, mounting it into the active region immediately if it's
/// the very first tab (so the switcher never starts out blank). The
/// sidebar Facet discovers new tabs itself by polling `list-tabs`,
/// rather than being told about each one as it's created. Shared by
/// `create_tab` (the `rashomon:browser` Host) and `Kernel::open_window`
/// (Facet Views) — both just hand this whichever `BrowserView` they
/// created, so a terminal session and a browser tab become
/// indistinguishable sidebar entries.
fn mount_tab_in_switcher(
    tab_id: String,
    browser_switcher: &Arc<Mutex<Option<BrowserSwitcherState>>>,
    browser_view: BrowserView,
) {
    let mut guard = browser_switcher.lock().expect("browser switcher lock poisoned");
    let Some(switcher) = guard.as_mut() else {
        eprintln!("mount-tab: no browser switcher window yet — tab created but not mounted anywhere");
        return;
    };

    let index = switcher.tabs.len();
    switcher.tabs.push((tab_id, browser_view.clone()));

    if index == 0 {
        let mut view = View::from(&browser_view);
        switcher.active_region.add_child_view(Some(&mut view));
        switcher.active_region.layout();
        switcher.active_index = 0;
    }
}

/// Out of 100 total — the sidebar gets `SIDEBAR_FLEX`% of the window's
/// width, content gets the rest. A *weighted* split like this doesn't
/// depend on either child's preferred size at all (`BoxLayout` only
/// consults preferred size for `flex: 0` items — distributing space by
/// weight among `flex > 0` items sidesteps that entirely), which
/// matters here because `BrowserView::GetPreferredSize()` doesn't
/// appear to honor its delegate's override the way a plain `Panel`
/// does. Confirmed empirically across two failed attempts: a `flex: 0`
/// sidebar collapsed to zero width; giving the window no layout
/// manager at all and setting bounds manually didn't stick either —
/// CEF re-runs some layout pass on its own (e.g. when the window
/// regains focus), clobbering manually-set bounds. A weighted split is
/// proportional rather than a fixed pixel width, but correct
/// regardless of window size, and doesn't fight CEF's own relayout.
const SIDEBAR_FLEX: i32 = 22;
const CONTENT_FLEX: i32 = 100 - SIDEBAR_FLEX;

/// Matches `TabWindowDelegate::preferred_size`'s 1024px window width —
/// only used for the draggable-region strip below, so it doesn't need
/// to track `SIDEBAR_FLEX` exactly (a slightly-off drag strip is a
/// cosmetic nit, not a correctness bug, unlike the sizing fiasco above).
const SIDEBAR_WIDTH_PX: i32 = 225;
/// Standard macOS traffic-light button height.
const TITLEBAR_STRIP_HEIGHT: i32 = 28;

/// Builds the one native window every tab (browser or Facet View) gets
/// mounted into and switched between — a direct port of
/// `cef-extension-spike`'s validated one-window, shared-active-region
/// design. The sidebar itself is *not* built here — it's a real Facet
/// Component (`components/sidebar`), mounted afterward via
/// [`Kernel::open_sidebar_view`] once that Facet has actually been
/// instantiated and rendered (see
/// `KernelBrowserProcessHandler::on_context_initialized`), so it's as
/// freely modifiable as any other Facet's HTML without touching this
/// native layer at all. This function only needs to reserve the
/// active-region side of the split and leave room (in the box layout's
/// child order — `root_panel.add_child_view_at(.., 0)`, see
/// `open_sidebar_view`) for the sidebar to slot in on the left.
fn open_browser_switcher_window(browser_switcher: &Arc<Mutex<Option<BrowserSwitcherState>>>) {
    let mut window_delegate = TabWindowDelegate::new();
    let Some(window) = window_create_top_level(Some(&mut window_delegate)) else {
        eprintln!("browser switcher: window_create_top_level returned None");
        return;
    };
    // The window's own direct child is a single root Panel (via
    // `FillLayout`, not `BoxLayout`) — two failed attempts (logged via
    // temporary bounds diagnostics) showed the *window's own*
    // `BoxLayout`, when a `BrowserView` is one of its direct children,
    // always gives that `BrowserView` the entire window regardless of
    // flex settings (bounds were logged as sidebar = full window,
    // active_region = zero, even immediately after an explicit
    // relayout). Nesting the real horizontal split one level deeper,
    // inside a plain `Panel` that is itself the *only* thing the
    // window manages, avoids whatever special-cased behavior the
    // window's root view has for a direct `BrowserView` child.
    window.set_to_fill_layout();

    let root_panel = panel_create(None).expect("panel_create failed");
    let root_layout = root_panel.set_to_box_layout(Some(&BoxLayoutSettings {
        horizontal: 1,
        // Default cross_axis_alignment is START, which sizes each
        // child to its own preferred size on the cross axis (height)
        // rather than filling the window — without this, the sidebar
        // and active-region panel (and its BrowserViews) collapse to
        // zero height. See `cef-extension-spike`'s identical fix.
        cross_axis_alignment: AxisAlignment::STRETCH,
        ..Default::default()
    }));

    let active_region = panel_create(None).expect("panel_create failed");
    active_region.set_to_fill_layout();

    let mut active_region_view = View::from(&active_region);
    root_panel.add_child_view(Some(&mut active_region_view));
    if let Some(root_layout) = &root_layout {
        root_layout.set_flex_for_view(Some(&mut active_region_view), CONTENT_FLEX);
    }
    root_panel.layout();

    *browser_switcher.lock().expect("browser switcher lock poisoned") = Some(BrowserSwitcherState {
        root_panel: root_panel.clone(),
        root_layout,
        active_region: active_region.clone(),
        tabs: Vec::new(),
        active_index: 0,
    });

    let mut root_panel_view = View::from(&root_panel);
    window.add_child_view(Some(&mut root_panel_view));
    window.layout();

    // Frameless (see `TabWindowDelegate::is_frameless`) means the OS
    // no longer has a titlebar to grab for moving the window — just
    // the sidebar's own top strip is draggable, matching where the
    // traffic-light buttons sit and leaving the rest of the sidebar
    // (its buttons) and all tab content click-through, not drag-through.
    window.set_draggable_regions(Some(&[DraggableRegion {
        bounds: Rect { x: 0, y: 0, width: SIDEBAR_WIDTH_PX, height: TITLEBAR_STRIP_HEIGHT },
        draggable: 1,
    }]));

    window.show();
}

wrap_display_handler! {
    struct TabDisplayHandler {
        title: Arc<Mutex<String>>,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            *self.title.lock().expect("title lock poisoned") =
                title.map(|t| t.to_string()).unwrap_or_default();
        }
    }
}

wrap_client! {
    struct TabClient {
        title: Arc<Mutex<String>>,
    }

    impl Client {
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(TabDisplayHandler::new(self.title.clone()))
        }
    }
}

wrap_window_delegate! {
    struct TabWindowDelegate {}

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size { width: 1024, height: 768 }
        }
    }
    impl PanelDelegate {}
    impl WindowDelegate {
        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn window_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::ALLOY
        }

        /// No native title bar — paired with `with_standard_window_buttons`
        /// below (macOS-only) so the traffic-light close/minimize/zoom
        /// buttons still render at the top-left, just without the bar
        /// itself. `Window::set_draggable_regions` (see
        /// `open_browser_switcher_window`) is what makes the window
        /// still movable without a titlebar to grab.
        fn is_frameless(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn with_standard_window_buttons(&self, _window: Option<&mut Window>) -> i32 {
            1
        }
    }
}

wrap_browser_view_delegate! {
    struct TabBrowserViewDelegate {}

    impl ViewDelegate {}
    impl BrowserViewDelegate {
        fn browser_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::ALLOY
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
            .map(|(id, view)| {
                // No separate per-tab title tracking yet (the page's
                // real `document.title` would need its own
                // `DisplayHandler`, wired uniformly across both
                // `create_tab`'s `TabClient` and Facet Views'
                // `KernelClient`) — the current URL doubles as the
                // label for now.
                let url = view
                    .browser()
                    .and_then(|b| b.main_frame())
                    .map(|f| CefString::from(&f.url()).to_string())
                    .unwrap_or_default();
                rashomon::browser::types::TabInfo { id: id.clone(), url: url.clone(), title: url }
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
        let browser = tab.browser_view.browser().ok_or("browser not ready yet")?;
        let frame = browser.main_frame().ok_or("tab has no main frame")?;
        frame.load_url(Some(&CefString::from(url.as_str())));
        Ok(())
    }

    fn current_url(&mut self, self_: Resource<BrowserTab>) -> String {
        let Ok(tab) = self.table.get(&self_) else { return String::new() };
        tab.browser_view
            .browser()
            .and_then(|b| b.main_frame())
            .map(|f| CefString::from(&f.url()).to_string())
            .unwrap_or_default()
    }

    fn title(&mut self, self_: Resource<BrowserTab>) -> String {
        let Ok(tab) = self.table.get(&self_) else { return String::new() };
        tab.title.lock().expect("title lock poisoned").clone()
    }

    fn close(&mut self, self_: Resource<BrowserTab>) {
        if let Ok(tab) = self.table.get(&self_) {
            if let Some(host) = tab.browser_view.browser().and_then(|b| b.host()) {
                host.close_browser(1);
            }
        }
    }

    fn snapshot_dom(&mut self, _self_: Resource<BrowserTab>) -> Result<String, String> {
        Err("not implemented yet".to_string())
    }

    fn inject_script(&mut self, self_: Resource<BrowserTab>, script: String) -> Result<String, String> {
        let tab = self.table.get(&self_).map_err(|e| e.to_string())?;
        let browser = tab.browser_view.browser().ok_or("browser not ready yet")?;
        let frame = browser.main_frame().ok_or("tab has no main frame")?;
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
            if let Some(host) = tab.browser_view.browser().and_then(|b| b.host()) {
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

    /// Mounts the new tab's `BrowserView` into the shared switcher
    /// window's active-region panel (making it visible immediately if
    /// it's the first tab ever created, exactly like
    /// `cef-extension-spike`'s `TabSwitcher` mounted its first tab at
    /// startup) rather than giving it a window of its own.
    fn create_tab(&mut self, _context: Resource<BrowserContext>, url: String) -> Resource<BrowserTab> {
        let title = Arc::new(Mutex::new(String::new()));
        let mut client = TabClient::new(title.clone());
        let settings = BrowserSettings::default();
        let cef_url = CefString::from(url.as_str());
        let mut browser_view_delegate = TabBrowserViewDelegate::new();
        let browser_view = browser_view_create(
            Some(&mut client),
            Some(&cef_url),
            Some(&settings),
            None,
            None,
            Some(&mut browser_view_delegate),
        )
        .expect("browser_view_create failed");

        let id = format!("tab-{}", next_tab_id());
        mount_tab_in_switcher(id.clone(), &self.browser_switcher, browser_view.clone());

        self.table
            .push(BrowserTab { id, browser_view, title })
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
struct InputBridge {
    store: Store<KernelState>,
    /// Set once in `on_context_initialized` and reused for every
    /// `browser_host_create_browser` call after that — one `Client` can
    /// back any number of Windows; CEF distinguishes them by the
    /// `browser`/`frame` each callback receives, not by which `Client`
    /// instance made the call. `None` only before the app's first
    /// Window exists.
    client: Option<Client>,
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

/// Everything needed to open a new View or mirror an existing one in a
/// new mounted tab: which Facets exist to instantiate, the live state
/// ([`InputBridge`]) either needs to register itself into, and the one
/// shared switcher ([`BrowserSwitcherState`]) every View's `BrowserView`
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
    /// [`open_browser_switcher_window`]) — the same as
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
    /// [`Kernel::open_sidebar_view`]) instead of the switchable
    /// `active_region` — for the "sidebar" Facet itself, which isn't a
    /// tab to switch away from. Called once, from
    /// `KernelBrowserProcessHandler::on_context_initialized`, after
    /// [`open_browser_switcher_window`] builds the window it mounts into.
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

    /// Instantiates the real `BrowserView` for View `view_id`'s current
    /// HTML, registering a fresh window id for cefQuery routing — the
    /// part [`Kernel::open_window`] (mounts into the switchable
    /// `active_region`) and [`Kernel::open_sidebar_view`] (mounts once,
    /// fixed) both need before deciding where the result goes. A real
    /// `BrowserView` (Views framework), not the raw
    /// `browser_host_create_browser` path — that's what lets this mount
    /// anywhere in [`BrowserSwitcherState`] instead of getting its own
    /// native window.
    fn create_view_browser_view(&self, view_id: &str) -> Result<(String, BrowserView)> {
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

        let mut client = bridge
            .client
            .clone()
            .ok_or_else(|| anyhow!("no Client yet — called before on_context_initialized?"))?;
        // Dropped before calling into CEF: `browser_view_create` can
        // turn around and call `LifeSpanHandler::on_after_created`
        // (which also locks `self.bridge`) before this function
        // returns, and `Mutex` isn't reentrant.
        drop(bridge);

        let settings = BrowserSettings::default();
        let url = CefString::from(html_data_uri(&inject_window_id(&html, &window_id)).as_str());
        let mut browser_view_delegate = TabBrowserViewDelegate::new();
        let browser_view = browser_view_create(
            Some(&mut client),
            Some(&url),
            Some(&settings),
            None,
            None,
            Some(&mut browser_view_delegate),
        )
        .ok_or_else(|| anyhow!("browser_view_create failed"))?;

        Ok((window_id, browser_view))
    }

    /// Mounts a new tab into the shared switcher window mirroring the
    /// already-running View `view_id`. Input typed into this tab
    /// reaches the exact same Facet instance (and so the same PTY, for
    /// `terminal-xterm`) as every other tab mirroring this View; output
    /// is fanned out to all of them via `pending` (see [`ViewHandle`]).
    fn open_window(&self, view_id: &str) -> Result<String> {
        let (window_id, browser_view) = self.create_view_browser_view(view_id)?;
        mount_tab_in_switcher(window_id.clone(), &self.browser_switcher, browser_view);
        Ok(window_id)
    }

    /// Mounts View `view_id` into the one fixed sidebar slot, to the
    /// left of `active_region` — not a switchable tab, so it doesn't go
    /// through [`mount_tab_in_switcher`] at all. `add_child_view_at(..,
    /// 0)` inserts it before `active_region` (added first, in
    /// [`open_browser_switcher_window`], since the sidebar wasn't ready
    /// yet) so it still ends up on the left.
    fn open_sidebar_view(&self, view_id: &str) -> Result<String> {
        let (window_id, browser_view) = self.create_view_browser_view(view_id)?;

        let mut guard = self.browser_switcher.lock().expect("browser switcher lock poisoned");
        let switcher = guard
            .as_mut()
            .ok_or_else(|| anyhow!("no browser switcher window yet"))?;
        let mut view = View::from(&browser_view);
        // Full teardown and rebuild — remove `active_region`, then
        // re-add both fresh, sidebar first — rather than
        // `add_child_view_at(.., 0)` to insert the sidebar before an
        // already-mounted, already-flexed sibling.
        let mut active_region_view = View::from(&switcher.active_region);
        switcher.root_panel.remove_child_view(Some(&mut active_region_view));
        switcher.root_panel.add_child_view(Some(&mut view));
        switcher.root_panel.add_child_view(Some(&mut active_region_view));
        if let Some(root_layout) = &switcher.root_layout {
            // Confirmed empirically (via bounds logging) that this
            // binding's `set_flex_for_view` assigns shares inverted
            // from every other CEF Views flex convention in this
            // codebase: `view` (the sidebar) needs `CONTENT_FLEX` to
            // end up with `SIDEBAR_FLEX`'s share, and vice versa for
            // `active_region`. Not a typo — verified three other
            // "natural" orderings all produced the exact swap this
            // avoids.
            root_layout.set_flex_for_view(Some(&mut view), CONTENT_FLEX);
            root_layout.set_flex_for_view(Some(&mut active_region_view), SIDEBAR_FLEX);
        }
        switcher.root_panel.layout();

        Ok(window_id)
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

impl BrowserSideHandler for InputQueryHandler {
    /// `request` is always `<window-id>:<event>` — every Window's page
    /// prefixes it that way before calling `window.cefQuery` (see
    /// `terminal-xterm`'s `render_page`) so this one handler, shared by
    /// every open Window, can dispatch to the right View. `event` is
    /// either [`POLL_REQUEST`] (answered from `poll-output`, fanned out
    /// to every Window mirroring this View — see [`ViewHandle`]) or
    /// real input (answered from `handle-input`, which reaches the
    /// exact same Facet instance no matter which mirroring Window sent
    /// it, so e.g. a PTY's own echo of typed input becomes output every
    /// mirroring Window's next poll picks up too).
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
        let Some((window_id, event)) = request.split_once(':') else {
            callback.failure(-1, "malformed request: missing <window-id>: prefix");
            return true;
        };

        let mut bridge = self.bridge.lock().expect("input bridge lock poisoned");
        let InputBridge { store, views, windows, .. } = &mut *bridge;
        let Some(view_id) = windows.get(window_id) else {
            callback.failure(-1, &format!("no such window: {window_id}"));
            return true;
        };
        let Some(view) = views.get_mut(view_id) else {
            callback.failure(-1, &format!("no such view: {view_id}"));
            return true;
        };

        if event == POLL_REQUEST {
            let result = view.bindings.rashomon_facet_contract().call_poll_output(store);
            match result {
                Ok(output) => {
                    if !output.is_empty() {
                        for pending in view.pending.values_mut() {
                            pending.push_str(&output);
                        }
                    }
                    let delivered = view.pending.get_mut(window_id).map(std::mem::take).unwrap_or_default();
                    callback.success_str(&delivered)
                }
                Err(e) => callback.failure(-1, &e.to_string()),
            }
            return true;
        }

        let result = view.bindings.rashomon_facet_contract().call_handle_input(store, event);
        match result {
            Ok(_) => callback.success_str("ok"),
            Err(e) => callback.failure(-1, &e.to_string()),
        }
        true
    }
}

wrap_client! {
    pub struct KernelClient {
        kernel: Arc<Kernel>,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(KernelLifeSpanHandler::new(self.kernel.bridge.clone()))
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
            let mut bridge = self.bridge.lock().expect("input bridge lock poisoned");
            bridge.browser_count -= 1;
            if bridge.browser_count == 0 {
                quit_message_loop();
            }
        }
    }
}

// `initial_views` is `(node_id, facet_name, window_count)` — each entry
// opens one View (one `node_id`/`facet_name` Facet instantiation) and
// then mirrors it into `window_count` total Windows via
// `Kernel::open_window`, so `window_count > 1` is how the "two Windows,
// one shared terminal session" demo in `run_browser_process` is
// expressed.
wrap_browser_process_handler! {
    struct KernelBrowserProcessHandler {
        kernel: Arc<Kernel>,
        initial_views: Arc<Vec<(String, String, u32)>>,
        browser_switcher: Arc<Mutex<Option<BrowserSwitcherState>>>,
        browser_component: Option<Arc<Component>>,
        browser_demo_node_id: String,
    }

    impl BrowserProcessHandler {
        /// Builds the one shared `Client`/`InputQueryHandler` pair every
        /// Window this process ever opens reuses, then opens every
        /// startup View and its Windows — each one a real
        /// `browser_host_create_browser` call via [`Kernel::open_view`]/
        /// [`Kernel::open_window`], so this doesn't need to wait on any
        /// page load the way the iframe-pane design (see the module doc
        /// comment) needed to.
        fn on_context_initialized(&self) {
            let client = KernelClient::new(self.kernel.clone());

            let router = BROWSER_ROUTER.get_or_init(|| BrowserSideRouter::new(message_router_config()));
            router.add_handler(
                Arc::new(InputQueryHandler {
                    bridge: self.kernel.bridge.clone(),
                }),
                false,
            );

            self.kernel.bridge.lock().expect("input bridge lock poisoned").client = Some(client);

            // Must happen before the `browser` Facet's `render()` call
            // below — `create-tab` needs `browser_switcher` to already
            // be `Some(..)` so it has somewhere to mount the tabs it
            // creates.
            open_browser_switcher_window(&self.browser_switcher);

            if let Some(browser_component) = &self.browser_component {
                let mut bridge = self.kernel.bridge.lock().expect("input bridge lock poisoned");
                let result = FacetWorld::instantiate(&mut bridge.store, browser_component, &self.kernel.facets.linker)
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
            // not inline host code — see `open_browser_switcher_window`'s
            // doc comment. Opened *last*, after real content is already
            // mounted into `active_region`: confirmed empirically
            // (three different attempts at the box-layout mechanics
            // all produced the exact same swapped 78/22 split) that
            // opening it while `active_region` was still empty — no
            // preferred/intrinsic size of its own to assert yet — let
            // the sidebar's own rendered page win the tug-of-war for
            // space regardless of flex settings.
            if let Err(e) = self.kernel.open_sidebar("sidebar-view", "sidebar") {
                eprintln!("failed to open sidebar: {e}");
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
        kernel: Option<Arc<Kernel>>,
        initial_views: Option<Arc<Vec<(String, String, u32)>>>,
        extension_configs: Arc<Vec<BrowserExtensionConfig>>,
        browser_switcher: Arc<Mutex<Option<BrowserSwitcherState>>>,
        browser_component: Option<Arc<Component>>,
        browser_demo_node_id: String,
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            let (Some(kernel), Some(initial_views)) = (self.kernel.clone(), self.initial_views.clone()) else {
                return None;
            };
            Some(KernelBrowserProcessHandler::new(
                kernel,
                initial_views,
                self.browser_switcher.clone(),
                self.browser_component.clone(),
                self.browser_demo_node_id.clone(),
            ))
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

/// An `App` with no `Kernel` yet — used for the initial `execute_process`
/// dispatch by *both* binaries, before we know whether this invocation is
/// the browser process (which gets a real `KernelApp` later, via
/// [`run_browser_process`]) or a subprocess (for which this is the only
/// `App` it will ever get, and the only thing that matters is that
/// `render_process_handler()` still works without a `Kernel`, which it
/// does).
pub fn make_minimal_app() -> App {
    KernelApp::new(
        None,
        None,
        Arc::new(Vec::new()),
        Arc::new(Mutex::new(None)),
        None,
        String::new(),
    )
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
        },
    );

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
    let thread_id = store
        .data_mut()
        .graph
        .create_node("rashomon:thread", rashomon_graph::Role::Entity, Default::default())
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
    let mirrored_thread = store
        .data_mut()
        .graph
        .create_node("rashomon:thread", rashomon_graph::Role::Entity, Default::default())
        .id;

    // A durable Entity for the browser Facet's `create-tab` calls to
    // attach their `rashomon:page` Occurrences to, same reasoning as
    // `thread_id` above.
    let browser_demo_node_id = store
        .data_mut()
        .graph
        .create_node("rashomon:thread", rashomon_graph::Role::Entity, Default::default())
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
        client: None,
        browser_count: 0,
        views: HashMap::new(),
        windows: HashMap::new(),
        next_view_id: 0,
        next_window_id: 0,
    }));
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

    let mut app = KernelApp::new(
        Some(kernel),
        Some(initial_views),
        extension_configs,
        browser_switcher,
        Some(Arc::new(browser_component)),
        browser_demo_node_id,
    );
    let settings = Settings {
        no_sandbox: 1,
        ..Default::default()
    };
    ensure!(
        initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) == 1,
        "cef initialize() failed"
    );

    #[cfg(target_os = "macos")]
    let _delegate = mac::setup_kernel_app_delegate();

    run_message_loop();
    shutdown();

    Ok(())
}
