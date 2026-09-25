//! Rashomon kernel — this pass only proves the Wasmtime Component
//! Model round-trip: build (if needed) and load the `ping` guest
//! component, supply a stub `rashomon:graph` host backed by
//! `InMemoryGraphStore`, call `render` once, and print the result.
//! No window, no UI, no real host loop yet.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use rashomon_graph::GraphStore;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "facet-world",
});

/// Host-side state for one component instance. Owns the in-memory
/// graph that backs the stub `rashomon:graph` implementation, plus the
/// WASI Preview 2 context that the cargo-component/wasm32-wasip1
/// toolchain's guest-side adapter always imports (even though the
/// `ping` guest never calls into it) — see the README for why that's
/// required just to instantiate.
struct KernelState {
    graph: rashomon_graph::InMemoryGraphStore,
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

impl rashomon::graph::graph::Host for KernelState {
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
        kind: String,
        source: String,
        target: String,
        confidence: f32,
    ) -> rashomon::graph::types::Edge {
        to_wit_edge(self.graph.create_edge(&kind, &source, &target, confidence))
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
        kind: edge.kind,
        source: edge.source,
        target: edge.target,
        timestamp: edge.timestamp,
        confidence: edge.confidence,
    }
}

/// Builds `components/ping` with `cargo component build` if its
/// compiled `.wasm` isn't already on disk, then returns the artifact
/// path.
fn ensure_ping_component_built() -> Result<PathBuf> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ping_dir = workspace_root.join("components/ping");
    let wasm_path = ping_dir.join("target/wasm32-wasip1/debug/ping.wasm");

    if !wasm_path.exists() {
        println!("ping component not built yet; running `cargo component build`...");
        let manifest_path = ping_dir.join("Cargo.toml");
        let status = Command::new("cargo")
            .args(["component", "build", "--manifest-path"])
            .arg(&manifest_path)
            .status()
            .context("failed to run `cargo component build` — is cargo-component installed?")?;
        if !status.success() {
            bail!("`cargo component build` for components/ping failed");
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

fn main() -> Result<()> {
    let wasm_path = ensure_ping_component_built()?;

    let engine = Engine::default();
    // `wasmtime::Error` doesn't implement `std::error::Error`, so
    // anyhow's `.context()`/`.with_context()` extension methods don't
    // apply directly to a `wasmtime::Result` — map it by hand instead.
   let component = Component::from_file(&engine, &wasm_path)
        .map_err(|e| anyhow!("failed to load component at {}: {e}", wasm_path.display()))?;

    let mut linker: Linker<KernelState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    FacetWorld::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;

    let mut store = Store::new(
        &engine,
        KernelState {
            graph: rashomon_graph::InMemoryGraphStore::new(),
            wasi_ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
        },
    );

    let bindings = FacetWorld::instantiate(&mut store, &component, &linker)
        .map_err(|e| anyhow!("failed to instantiate ping component: {e}"))?;

    let rendered = bindings
        .rashomon_facet_facet()
        .call_render(&mut store, "demo-node")
        .map_err(|e| anyhow!("call to render() failed: {e}"))?;

    println!("ping component rendered: {rendered}");

    Ok(())
}
