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
//! any Entity and give it its own top-level CEF Window (one real
//! `browser_host_create_browser` call per View, all sharing one
//! `Client`/`InputQueryHandler`/`Store` via [`InputBridge`]), which is
//! not what was originally planned here.
//!
//! **Why not one Window with multiple Views as panes:** the first
//! attempt loaded a single shell page and inserted each View as an
//! `<iframe srcdoc>` pane via `execute_java_script`. Two separate bugs
//! showed up chasing that down, both confirmed empirically rather than
//! assumed: (1) `execute_java_script` silently does nothing when called
//! from a CEF `Task` (e.g. one scheduled via `post_delayed_task`) —
//! identical script, called from a genuine Client/Handler callback like
//! `on_load_end`, ran and logged; called from `Task::execute()`,
//! returned normally but never actually ran. That one's dodged below by
//! routing output through the same `cefQuery` round trip input already
//! uses ([`POLL_REQUEST`]) instead of a host-side push loop. (2) More
//! fundamentally, dynamically-created `<iframe srcdoc>` elements never
//! finished navigating in this CEF/Alloy configuration at all — not
//! even a fully offline, dependency-free one, and not even one declared
//! statically in the page's own initial HTML (which additionally
//! blocked the *parent* page's own `on_load_end` from ever firing, since
//! that normally waits on all initial subresources). Real multi-pane
//! support belongs to native child-view embedding (`WindowInfo`'s
//! `parent_view`/`bounds`, the same mechanism `cefclient`'s own
//! multi-pane UI uses) instead of HTML iframes — a real enough chunk of
//! per-platform work that it's deliberately left as the next concrete
//! step, not something to half-do here.
//!
//! Input flows back via a CEF message-router bridge: each Window's page
//! prefixes its `window.cefQuery` calls with its own window id, so one
//! shared [`InputQueryHandler`] can route a keystroke (or a
//! [`POLL_REQUEST`]) to the right View's `handle-input` (or
//! `poll-output`) rather than there being one handler per Window. More
//! than one Window can mirror the same View this way — see
//! [`Kernel::open_window`] — with output fanned out so no mirroring
//! Window loses it to whichever one happens to poll first. Every
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
/// new Window: which Facets exist to instantiate, and the live state
/// ([`InputBridge`]) either needs to register itself into.
struct Kernel {
    facets: FacetRegistry,
    bridge: Arc<Mutex<InputBridge>>,
}

impl Kernel {
    /// Instantiates `facet_name`'s Component fresh, calls its `render`
    /// against `node_id` to get its first View, and opens that View in
    /// a brand-new top-level CEF Window (see the module doc comment for
    /// why this is a real Window rather than a pane in a shared one,
    /// for now) — the same as [`Kernel::open_window`] would for any
    /// later Window mirroring this View, just with a fresh View instead
    /// of an existing one. Safe to call more than once against the same
    /// Facet: each call is an independent instantiation (independent
    /// `SESSION`-style guest state), the same way opening two terminal
    /// windows in a real OS gives you two independent shells, not one
    /// shared one — [`Kernel::open_window`] is what shares one.
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

    /// Opens a new top-level CEF Window mirroring the already-running
    /// View `view_id`: same `initial_html`, a fresh window id spliced
    /// in. Input typed into this Window reaches the exact same Facet
    /// instance (and so the same PTY, for `terminal-xterm`) as every
    /// other Window mirroring this View; output is fanned out to all of
    /// them via `pending` (see [`ViewHandle`]).
    fn open_window(&self, view_id: &str) -> Result<String> {
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
        // Dropped before calling into CEF: `browser_host_create_browser`
        // can turn around and call `LifeSpanHandler::on_after_created`
        // (which also locks `self.bridge`) before this function
        // returns, and `Mutex` isn't reentrant.
        drop(bridge);

        let window_info = WindowInfo {
            runtime_style: RuntimeStyle::ALLOY,
            ..Default::default()
        };
        let settings = BrowserSettings::default();
        let url = CefString::from(html_data_uri(&inject_window_id(&html, &window_id)).as_str());
        browser_host_create_browser(
            Some(&window_info),
            Some(&mut client),
            Some(&url),
            Some(&settings),
            None,
            None,
        );

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
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            let (Some(kernel), Some(initial_views)) = (self.kernel.clone(), self.initial_views.clone()) else {
                return None;
            };
            Some(KernelBrowserProcessHandler::new(kernel, initial_views))
        }

        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(KernelRenderProcessHandler::new())
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
    KernelApp::new(None, None)
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
    let graph_db_path = graph_db_path()?;

    let engine = Engine::default();
    let mut linker: Linker<KernelState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    FacetWorld::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;

    println!("graph database: {}", graph_db_path.display());

    let mut store = Store::new(
        &engine,
        KernelState {
            graph: rashomon_graph::PersistentGraphStore::open(&graph_db_path),
            wasi_ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
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

    let terminal_xterm_component =
        Component::from_file(&engine, &terminal_xterm_wasm_path).map_err(|e| {
            anyhow!(
                "failed to load component at {}: {e}",
                terminal_xterm_wasm_path.display()
            )
        })?;

    let mut facets = HashMap::new();
    facets.insert("terminal-xterm".to_string(), terminal_xterm_component);

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
    });

    // One `terminal-xterm` View, mirrored into two Windows: typing in
    // either reaches the same PTY, and output (including the PTY's own
    // echo of that input) is fanned out to both — see `ViewHandle` and
    // `Kernel::open_window`.
    let initial_views = Arc::new(vec![(mirrored_thread, "terminal-xterm".to_string(), 2u32)]);

    let mut app = KernelApp::new(Some(kernel), Some(initial_views));
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
