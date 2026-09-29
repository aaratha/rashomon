//! Rashomon kernel — proves the Wasmtime Component Model round-trip
//! against several guests: `ping` (minimal render-only demo), `shell`
//! (one-shot process spawn + graph write), `terminal` (a real
//! interactive PTY-backed terminal, parsed guest-side with `vt100` — the
//! live View shown in the window), and `terminal-xterm` (the same PTY,
//! but handing the host an `xterm.js` HTML payload instead of parsing
//! it itself — proven via a one-shot render() call only, since there's
//! no CEF/`rashomon:ui` yet to actually run that HTML). Every primitive
//! is backed by a real host implementation — `rashomon:graph` by a
//! `PersistentGraphStore` (so the graph survives a restart), and
//! `rashomon:process` by a real PTY (`portable-pty`), not plain OS
//! pipes — `resize`/`signal` are genuine now. A real winit window +
//! event loop keeps the process alive and feeds keyboard input into
//! `handle-input`; it draws plain bitmap text for now — most real UI is
//! expected to eventually render through CEF (the same Native Backend
//! as `rashomon:browser`), not this software rasterizer.

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use directories::ProjectDirs;
use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use rashomon_graph::GraphStore;
use softbuffer::{Context as SoftbufferContext, Surface};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::window::{Window, WindowId};

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

/// Turns a raw keyboard event into the byte sequence a terminal
/// actually expects — the same job a real terminal emulator's key
/// encoder does. Deliberately simple (no mode-dependent cursor-key
/// encoding, no libghostty/alacritty `KeyEncoder`): Ctrl+letter maps to
/// its standard ASCII control code (Ctrl+C -> 0x03, matching how a real
/// PTY's line discipline expects to receive an interrupt), a handful of
/// named keys map to their common escape sequences, and anything else
/// with a printable representation is forwarded as-is.
fn encode_key_event(
    logical_key: &winit::keyboard::Key,
    modifiers: winit::keyboard::ModifiersState,
) -> Option<Vec<u8>> {
    use winit::keyboard::{Key, NamedKey};

    if modifiers.control_key() {
        if let Key::Character(s) = logical_key {
            if let Some(c) = s.chars().next().filter(|c| c.is_ascii_alphabetic()) {
                return Some(vec![c.to_ascii_uppercase() as u8 - b'A' + 1]);
            }
        }
    }

    match logical_key {
        Key::Character(s) => Some(s.as_bytes().to_vec()),
        Key::Named(NamedKey::Enter) => Some(vec![b'\r']),
        Key::Named(NamedKey::Backspace) => Some(vec![0x7f]),
        Key::Named(NamedKey::Tab) => Some(vec![b'\t']),
        Key::Named(NamedKey::Escape) => Some(vec![0x1b]),
        Key::Named(NamedKey::Space) => Some(vec![b' ']),
        Key::Named(NamedKey::ArrowUp) => Some(b"\x1b[A".to_vec()),
        Key::Named(NamedKey::ArrowDown) => Some(b"\x1b[B".to_vec()),
        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
        Key::Named(NamedKey::ArrowLeft) => Some(b"\x1b[D".to_vec()),
        Key::Named(NamedKey::Delete) => Some(b"\x1b[3~".to_vec()),
        Key::Named(NamedKey::Home) => Some(b"\x1b[H".to_vec()),
        Key::Named(NamedKey::End) => Some(b"\x1b[F".to_vec()),
        _ => None,
    }
}

/// The host loop's live View: a real window showing the `terminal`
/// Facet's current screen contents, redrawn on a short poll interval
/// (not just on keypress) so output from a long-running command shows
/// up on its own — plus a software-rendered solid background for now,
/// see the module doc comment. Owns the `Store` and the `terminal`
/// bindings so `about_to_wait` can poll `render` and `window_event` can
/// forward encoded keystrokes to `handle-input`.
struct App {
    store: Store<KernelState>,
    terminal_bindings: FacetWorld,
    thread_id: String,
    screen_text: String,
    modifiers: winit::keyboard::ModifiersState,
    window: Option<Rc<Window>>,
    surface: Option<Surface<OwnedDisplayHandle, Rc<Window>>>,
}

/// Matches the PTY's fixed initial size in `spawner::Host::spawn`, so
/// the window opens sized to fit exactly this many rows/cols with no
/// wasted or clipped space.
const TERMINAL_ROWS: u32 = 24;
const TERMINAL_COLS: u32 = 80;
const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const RASTER_HEIGHT: RasterHeight = RasterHeight::Size16;

/// Blits `text` (rows separated by `\n`) into `buffer` as monospaced
/// bitmap glyphs — light text on the same dark background as the old
/// solid-fill placeholder, alpha-blended per-pixel from each glyph's
/// grayscale raster rather than drawn as flat on/off pixels.
fn draw_terminal_text(buffer: &mut [u32], width: usize, height: usize, text: &str) {
    const BG: (u32, u32, u32) = (0x20, 0x28, 0x30);
    const FG: (u32, u32, u32) = (0xe0, 0xe0, 0xe0);

    buffer.fill((BG.0 << 16) | (BG.1 << 8) | BG.2);

    let char_width = get_raster_width(FONT_WEIGHT, RASTER_HEIGHT);
    let char_height = RASTER_HEIGHT.val();

    for (row, line) in text.lines().enumerate() {
        let y0 = row * char_height;
        if y0 >= height {
            break;
        }
        for (col, ch) in line.chars().enumerate() {
            let x0 = col * char_width;
            if x0 >= width {
                break;
            }
            let Some(glyph) = get_raster(ch, FONT_WEIGHT, RASTER_HEIGHT) else {
                continue;
            };
            for (dy, glyph_row) in glyph.raster().iter().enumerate() {
                let y = y0 + dy;
                if y >= height {
                    break;
                }
                for (dx, &intensity) in glyph_row.iter().enumerate() {
                    let x = x0 + dx;
                    if x >= width {
                        break;
                    }
                    let a = intensity as u32;
                    let r = (BG.0 * (255 - a) + FG.0 * a) / 255;
                    let g = (BG.1 * (255 - a) + FG.1 * a) / 255;
                    let b = (BG.2 * (255 - a) + FG.2 * a) / 255;
                    buffer[y * width + x] = (r << 16) | (g << 8) | b;
                }
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let char_width = get_raster_width(FONT_WEIGHT, RASTER_HEIGHT) as u32;
        let char_height = RASTER_HEIGHT.val() as u32;
        let window = Rc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Rashomon terminal")
                        .with_inner_size(winit::dpi::LogicalSize::new(
                            TERMINAL_COLS * char_width,
                            TERMINAL_ROWS * char_height,
                        )),
                )
                .expect("failed to create window"),
        );
        let context = SoftbufferContext::new(event_loop.owned_display_handle())
            .expect("failed to create softbuffer context");
        let surface =
            Surface::new(&context, window.clone()).expect("failed to create softbuffer surface");
        self.window = Some(window);
        self.surface = Some(surface);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let rendered = self
            .terminal_bindings
            .rashomon_facet_contract()
            .call_render(&mut self.store, &self.thread_id)
            .unwrap_or_else(|e| format!("terminal: render() failed: {e}"));
        if rendered != self.screen_text {
            self.screen_text = rendered;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(50),
        ));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if id != window.id() {
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                let Some(surface) = &mut self.surface else {
                    return;
                };
                let size = window.inner_size();
                let (Some(width), Some(height)) =
                    (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                else {
                    return;
                };
                surface.resize(width, height).expect("failed to resize surface");
                let mut buffer = surface.buffer_mut().expect("failed to get window buffer");
                draw_terminal_text(
                    &mut buffer,
                    size.width as usize,
                    size.height as usize,
                    &self.screen_text,
                );
                buffer.present().expect("failed to present window buffer");
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent {
                    state: ElementState::Pressed,
                    logical_key,
                    ..
                },
                ..
            } => {
                let Some(bytes) = encode_key_event(&logical_key, self.modifiers) else {
                    return;
                };
                // Terminal input bytes (printable chars, ASCII control
                // codes, common escape sequences) are always valid
                // UTF-8 here, so this round-trip through `String` (the
                // facet contract's placeholder payload type) is lossless.
                let encoded = String::from_utf8(bytes).expect("encoded key bytes are valid UTF-8");
                let _ = self
                    .terminal_bindings
                    .rashomon_facet_contract()
                    .call_handle_input(&mut self.store, &encoded)
                    .expect("call to terminal's handle_input() failed");
            }
            _ => {}
        }
    }
}

fn main() -> Result<()> {
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
    // document for `xterm.js` to run, not host-parsed text. Proven only
    // via this one-shot call — there's no CEF/`rashomon:ui` yet to
    // actually load and run that HTML, so it can't be the window's live
    // View the way `terminal` is.
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
    println!(
        "terminal-xterm component rendered {} bytes of HTML, starting: {:?}...",
        xterm_page.len(),
        &xterm_page[..xterm_page.len().min(120)]
    );

    // Hand off to a real, persistent host loop — `terminal` is the live
    // View shown in the window; typing feeds `handle-input`, and a
    // short poll interval keeps the display current even when nothing
    // was typed (e.g. a long-running command's output).
    let event_loop = EventLoop::new().context("failed to create winit event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App {
        store,
        terminal_bindings,
        thread_id,
        screen_text: initial_screen,
        modifiers: winit::keyboard::ModifiersState::empty(),
        window: None,
        surface: None,
    };
    event_loop
        .run_app(&mut app)
        .context("winit event loop exited with an error")?;

    Ok(())
}
