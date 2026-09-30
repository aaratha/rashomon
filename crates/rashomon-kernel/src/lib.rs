//! Rashomon kernel — proves the Wasmtime Component Model round-trip
//! against several guests: `ping` (minimal render-only demo), `shell`
//! (one-shot process spawn + graph write), `terminal` (a real
//! interactive PTY-backed terminal, parsed guest-side with `vt100` —
//! proven via a one-shot render() call, console-only for now), and
//! `terminal-xterm` (the same PTY, handing the host an `xterm.js` HTML
//! payload — the window's actual live content, with real input flowing
//! back via a CEF message-router bridge: `term.onData` ->
//! `window.cefQuery` -> this crate's `InputQueryHandler` ->
//! `handle-input`, and live output flowing the other way via a
//! recurring `OutputPollTask` that calls `poll-output` and pushes any
//! new bytes into the page with `execute_java_script`). Every
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

use std::cell::RefCell;
use std::collections::VecDeque;
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

/// Shared with the browser-side query handler so a `window.cefQuery`
/// call from the page's JS can reach all the way into a real
/// `handle-input` call on the Component that produced that page.
struct InputBridge {
    store: Store<KernelState>,
    terminal_xterm_bindings: FacetWorld,
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

impl BrowserSideHandler for InputQueryHandler {
    fn on_query_str(
        &self,
        _browser: Option<Browser>,
        _frame: Option<Frame>,
        _query_id: i64,
        request: &str,
        _persistent: bool,
        callback: Arc<Mutex<dyn BrowserSideCallback>>,
    ) -> bool {
        let mut bridge = self.bridge.lock().expect("input bridge lock poisoned");
        let InputBridge { store, terminal_xterm_bindings } = &mut *bridge;
        let result = terminal_xterm_bindings
            .rashomon_facet_contract()
            .call_handle_input(store, request);
        let callback = callback.lock().expect("callback lock poisoned");
        match result {
            Ok(_) => callback.success_str("ok"),
            Err(e) => callback.failure(-1, &e.to_string()),
        }
        true
    }
}

wrap_client! {
    pub struct KernelClient {
        inner: Arc<Mutex<KernelWindowState>>,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(KernelLifeSpanHandler::new(self.inner.clone()))
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

struct KernelWindowState {
    browser_count: u32,
    /// Set once the browser exists, so the output-poll task (see
    /// [`OutputPollTask`]) has something to call `execute_java_script`
    /// on — it starts running before `on_after_created` fires, so it
    /// has to tolerate this being `None` for its first tick or two.
    browser: Option<Browser>,
}

wrap_life_span_handler! {
    struct KernelLifeSpanHandler {
        inner: Arc<Mutex<KernelWindowState>>,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let mut state = self.inner.lock().expect("lock poisoned");
            state.browser_count += 1;
            state.browser = browser.cloned();
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            if let Some(router) = BROWSER_ROUTER.get() {
                router.on_before_close(browser.cloned());
            }
            let mut state = self.inner.lock().expect("lock poisoned");
            state.browser_count -= 1;
            if state.browser_count == 0 {
                quit_message_loop();
            }
        }
    }
}

wrap_browser_process_handler! {
    struct KernelBrowserProcessHandler {
        client: RefCell<Option<Client>>,
        html: String,
        input_bridge: Arc<Mutex<InputBridge>>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            let window_state = Arc::new(Mutex::new(KernelWindowState {
                browser_count: 0,
                browser: None,
            }));
            let mut client = KernelClient::new(window_state.clone());
            *self.client.borrow_mut() = Some(client.clone());

            let router = BROWSER_ROUTER.get_or_init(|| BrowserSideRouter::new(message_router_config()));
            router.add_handler(
                Arc::new(InputQueryHandler {
                    bridge: self.input_bridge.clone(),
                }),
                false,
            );

            let window_info = WindowInfo {
                runtime_style: RuntimeStyle::ALLOY,
                ..Default::default()
            };
            let settings = BrowserSettings::default();
            let url = CefString::from(html_data_uri(&self.html).as_str());

            browser_host_create_browser(
                Some(&window_info),
                Some(&mut client),
                Some(&url),
                Some(&settings),
                None,
                None,
            );

            let mut poll_task = OutputPollTask::new(self.input_bridge.clone(), window_state);
            post_delayed_task(ThreadId::UI, Some(&mut poll_task), POLL_INTERVAL_MS);
        }
    }
}

/// How often the poll loop checks for new PTY output and pushes it into
/// the page. Short enough to feel live, long enough not to spin the UI
/// thread — this is a placeholder until there's a real `rashomon:ui`
/// primitive with its own event-driven update story.
const POLL_INTERVAL_MS: i64 = 33;

wrap_task! {
    struct OutputPollTask {
        bridge: Arc<Mutex<InputBridge>>,
        window_state: Arc<Mutex<KernelWindowState>>,
    }

    impl Task {
        /// Calls `terminal-xterm`'s `poll-output`, and if it drained any
        /// new PTY bytes, pushes them into the already-loaded page via
        /// `execute_java_script` — base64-encoded so arbitrary bytes
        /// (quotes, control characters) survive being embedded in a JS
        /// string literal without hand-rolled escaping, and decoded back
        /// into a `Uint8Array` of the original bytes in JS (rather than
        /// treating `atob`'s Latin1-per-byte string as the text
        /// directly) so xterm.js still sees genuine UTF-8, not mangled
        /// multi-byte characters. Always reschedules itself, whether or
        /// not there was anything to push, so the loop keeps running for
        /// the life of the window.
        fn execute(&self) {
            let browser = self
                .window_state
                .lock()
                .expect("window state lock poisoned")
                .browser
                .clone();

            if let Some(frame) = browser.and_then(|b| b.main_frame()) {
                let mut bridge = self.bridge.lock().expect("input bridge lock poisoned");
                let InputBridge { store, terminal_xterm_bindings } = &mut *bridge;
                let output = terminal_xterm_bindings
                    .rashomon_facet_contract()
                    .call_poll_output(store);
                drop(bridge);

                if let Ok(output) = output {
                    if !output.is_empty() {
                        let b64 = CefString::from(&base64_encode(Some(output.as_bytes()))).to_string();
                        let js = format!(
                            "term.write(Uint8Array.from(atob('{b64}'), c => c.charCodeAt(0)));"
                        );
                        frame.execute_java_script(Some(&CefString::from(js.as_str())), None, 0);
                    }
                }
            }

            let mut next = OutputPollTask::new(self.bridge.clone(), self.window_state.clone());
            post_delayed_task(ThreadId::UI, Some(&mut next), POLL_INTERVAL_MS);
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
        html: Option<String>,
        input_bridge: Option<Arc<Mutex<InputBridge>>>,
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            let (Some(html), Some(input_bridge)) = (self.html.clone(), self.input_bridge.clone()) else {
                return None;
            };
            Some(KernelBrowserProcessHandler::new(RefCell::new(None), html, input_bridge))
        }

        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(KernelRenderProcessHandler::new())
        }
    }
}

/// An `App` with no html/input_bridge yet — used for the initial
/// `execute_process` dispatch by *both* binaries, before we know
/// whether this invocation is the browser process (which gets a real
/// `KernelApp` later, via [`run_browser_process`]) or a subprocess
/// (for which this is the only `App` it will ever get, and the only
/// thing that matters is that `render_process_handler()` still works
/// without html/input_bridge, which it does).
pub fn make_minimal_app() -> App {
    KernelApp::new(None, None)
}

/// Everything that happens once we know this process is the CEF browser
/// process: build/instantiate the demo Components (`ping`/`shell`/
/// `terminal`/`terminal-xterm`), then hand off to CEF's own run loop.
/// Blocks until the window is closed.
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

    // `terminal-xterm`: same PTY, but the render() payload is an HTML
    // document for `xterm.js` to run. CEF loads it below as the
    // window's actual content.
    let terminal_xterm_component =
        Component::from_file(&engine, &terminal_xterm_wasm_path).map_err(|e| {
            anyhow!(
                "failed to load component at {}: {e}",
                terminal_xterm_wasm_path.display()
            )
        })?;
    let terminal_xterm_bindings =
        FacetWorld::instantiate(&mut store, &terminal_xterm_component, &linker)
            .map_err(|e| anyhow!("failed to instantiate terminal-xterm component: {e}"))?;
    let xterm_page = terminal_xterm_bindings
        .rashomon_facet_contract()
        .call_render(&mut store, &thread_id)
        .map_err(|e| anyhow!("call to terminal-xterm's render() failed: {e}"))?;
    println!("terminal-xterm component rendered {} bytes of HTML", xterm_page.len());

    // `store` and `terminal_xterm_bindings` move into the input bridge
    // here — real interactivity (`term.onData` -> `window.cefQuery` ->
    // `handle-input`) reaches them through `InputQueryHandler` from now
    // on, not through any further direct use in this function.
    let input_bridge = Arc::new(Mutex::new(InputBridge {
        store,
        terminal_xterm_bindings,
    }));

    let mut app = KernelApp::new(Some(xterm_page), Some(input_bridge));
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
