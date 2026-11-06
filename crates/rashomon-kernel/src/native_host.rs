//! Firefox native-messaging entry point for the Firefox-extension Host
//! pivot (see `docs/FIREFOX_EXTENSION_SPIKE.md`). Firefox spawns this
//! binary once per `browser.runtime.connectNative()` call and talks to
//! it over stdin/stdout using Firefox's native messaging protocol: each
//! message is a 4-byte little-endian length prefix followed by that
//! many bytes of UTF-8 JSON, in both directions. Kept as its own
//! `[[bin]]` rather than a CLI flag on `rashomon-kernel`'s existing
//! entry point, which assumes it's already the CEF browser process
//! (see `main.rs`) and must never be launched that way by Firefox.
//!
//! Stage 1: mirrors each relayed tab into the graph as a
//! `rashomon:page` Entity (identity resolved by exact URL match — the
//! real identity-resolution ladder from the design doc §5 is deferred)
//! plus a `rashomon:browser-tab` Occurrence `occurrence-of` it, and
//! maps Sidebery's own tab-tree parent (relayed per event, since
//! Sidebery resolves it asynchronously against its sidebar — see
//! `native-host.bg.ts` in the fork) onto a `contains` edge between the
//! two tabs' page Entities. That edge is deliberately permanent: it
//! survives the tab closing, the same way everything else in the graph
//! outlives whatever produced it. Doesn't touch `rashomon:ui` — that's
//! Stage 2+.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use directories::ProjectDirs;
use rashomon_graph::{GraphStore, PersistentGraphStore, Role};
use serde_json::Value;

/// Firefox gives native hosts no terminal and discards stderr, so a
/// log file under the same data dir the kernel already uses (see
/// `extensions_config_path` in `lib.rs`) is the only way to observe
/// this process while developing against it.
fn log_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("dev", "rashomon", "rashomon")?;
    let data_dir = dirs.data_dir();
    std::fs::create_dir_all(data_dir).ok()?;
    Some(data_dir.join("native-host.log"))
}

fn log(line: &str) {
    if let Some(path) = log_path() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn read_message(stdin: &mut io::Stdin) -> io::Result<Option<Value>> {
    let mut len_buf = [0u8; 4];
    match stdin.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stdin.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

fn write_message(stdout: &mut io::Stdout, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).expect("Value always serializes");
    let len = (bytes.len() as u32).to_le_bytes();
    stdout.write_all(&len)?;
    stdout.write_all(&bytes)?;
    stdout.flush()
}

/// Graph node ids already created for one open tab, keyed by Sidebery's
/// `tabId` for the lifetime of this process (a fresh process per
/// `connectNative` connection — no attempt to reconcile against
/// already-mirrored tabs from a previous connection; that's reconciling
/// live browser state against the graph on startup, a bigger problem
/// than this spike needs to solve).
struct TabNodes {
    page_id: String,
    occurrence_id: String,
}

struct Mirror {
    graph: PersistentGraphStore,
    tabs: HashMap<i64, TabNodes>,
}

impl Mirror {
    fn open() -> anyhow::Result<Self> {
        let path = rashomon_kernel::graph_db_path()?;
        Ok(Self {
            graph: PersistentGraphStore::open(path),
            tabs: HashMap::new(),
        })
    }

    /// Exact-URL dedup only — reuses a `rashomon:page` Entity already
    /// in the graph (from a previous tab, or a previous run) rather
    /// than creating a duplicate for every tab pointed at the same URL.
    fn find_or_create_page(&mut self, url: &str) -> String {
        let existing = self.graph.all_nodes().into_iter().find(|n| {
            n.node_type == "rashomon:page" && n.properties.get("url").map(String::as_str) == Some(url)
        });
        if let Some(node) = existing {
            return node.id;
        }
        let mut props = HashMap::new();
        props.insert("url".to_string(), url.to_string());
        self.graph.create_node("rashomon:page", Role::Entity, props).id
    }

    /// Creates the page Entity + browser-tab Occurrence pair for a tab
    /// the first time it's seen with a usable URL; a no-op on every
    /// later event for the same tab (there's no `update_node` on
    /// `GraphStore` to refresh title/url on navigation — an honest gap
    /// for this spike, not this file's to fix).
    fn ensure_tab(&mut self, tab_id: i64, url: &str, title: Option<&str>, window_id: i64) {
        if self.tabs.contains_key(&tab_id) {
            return;
        }
        let page_id = self.find_or_create_page(url);
        let mut props = HashMap::new();
        props.insert("tab_id".to_string(), tab_id.to_string());
        props.insert("window_id".to_string(), window_id.to_string());
        if let Some(t) = title {
            props.insert("title".to_string(), t.to_string());
        }
        let occurrence = self.graph.create_node("rashomon:browser-tab", Role::Occurrence, props);
        self.graph.create_edge("occurrence-of", &occurrence.id, &page_id, 1.0);
        self.tabs.insert(
            tab_id,
            TabNodes {
                page_id,
                occurrence_id: occurrence.id,
            },
        );
    }

    /// Maps Sidebery's tab-tree parent onto a `contains` edge between
    /// the parent and child tabs' page Entities (Entity -> Entity, per
    /// the design doc §4) — skipped until both tabs have been mirrored,
    /// and deduped against an edge already created by an earlier event
    /// for the same pair (both tabs fire `onUpdated` repeatedly).
    fn link_parent(&mut self, tab_id: i64, parent_tab_id: i64) {
        if parent_tab_id < 0 || parent_tab_id == tab_id {
            return;
        }
        let Some(child_page) = self.tabs.get(&tab_id).map(|t| t.page_id.clone()) else {
            return;
        };
        let Some(parent_page) = self.tabs.get(&parent_tab_id).map(|t| t.page_id.clone()) else {
            return;
        };
        if child_page == parent_page {
            return;
        }
        let already_linked = self
            .graph
            .query_edges_to(&child_page)
            .iter()
            .any(|e| e.edge_type == "contains" && e.source == parent_page);
        if !already_linked {
            self.graph.create_edge("contains", &parent_page, &child_page, 1.0);
        }
    }

    /// The page Entity persists (it's the lasting record of the URL);
    /// only the Occurrence representing "this tab was open" goes away.
    fn handle_removed(&mut self, tab_id: i64) {
        if let Some(nodes) = self.tabs.remove(&tab_id) {
            self.graph.delete_node(&nodes.occurrence_id);
        }
    }

    fn handle_event(&mut self, msg: &Value) {
        let event_type = msg.get("type").and_then(Value::as_str).unwrap_or_default();
        let Some(tab_id) = msg.get("tabId").and_then(Value::as_i64) else {
            return;
        };

        if event_type == "tabs.onRemoved" {
            self.handle_removed(tab_id);
            return;
        }

        if let Some(url) = msg.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()) {
            let title = msg.get("title").and_then(Value::as_str);
            let window_id = msg.get("windowId").and_then(Value::as_i64).unwrap_or(-1);
            self.ensure_tab(tab_id, url, title, window_id);
        }

        if let Some(parent_id) = msg.get("parentId").and_then(Value::as_i64) {
            self.link_parent(tab_id, parent_id);
        }
    }
}

fn main() {
    log(&format!("--- native host started, pid {} ---", std::process::id()));

    let mut mirror = match Mirror::open() {
        Ok(m) => m,
        Err(e) => {
            log(&format!("failed to open graph, exiting: {e}"));
            return;
        }
    };

    let mut stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        match read_message(&mut stdin) {
            Ok(Some(message)) => {
                log(&format!("recv: {message}"));
                mirror.handle_event(&message);
                let ack = serde_json::json!({ "ack": true, "received": message });
                if let Err(e) = write_message(&mut stdout, &ack) {
                    log(&format!("write failed, exiting: {e}"));
                    break;
                }
            }
            Ok(None) => {
                log("stdin closed, exiting");
                break;
            }
            Err(e) => {
                log(&format!("read failed, exiting: {e}"));
                break;
            }
        }
    }
}
