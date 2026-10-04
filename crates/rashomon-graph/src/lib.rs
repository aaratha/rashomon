//! Plain Rust graph types mirroring `wit/deps/graph.wit`, a `GraphStore`
//! trait, and two implementations: `InMemoryGraphStore` (a `HashMap`,
//! gone on process exit — fast, used in tests) and `PersistentGraphStore`
//! (a `petgraph` index backed by a `redb` file, so the graph survives a
//! restart). See `rashomon-architecture.md` sections 1, 3, 4.

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const NODES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("nodes");
const EDGES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("edges");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Entity,
    Occurrence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub node_type: String,
    pub role: Role,
    pub properties: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub edge_type: String,
    pub source: String,
    pub target: String,
    pub timestamp: u64,
    pub confidence: f32,
}

pub trait GraphStore {
    fn create_node(&mut self, node_type: &str, role: Role, properties: HashMap<String, String>) -> Node;
    fn get_node(&self, id: &str) -> Option<Node>;
    fn create_edge(&mut self, edge_type: &str, source: &str, target: &str, confidence: f32) -> Edge;
    fn query_edges_from(&self, node_id: &str) -> Vec<Edge>;
    fn query_edges_to(&self, node_id: &str) -> Vec<Edge>;
    /// Every Node that currently exists — needed by anything that
    /// browses/visualizes the whole graph rather than following edges
    /// from an already-known starting point.
    fn all_nodes(&self) -> Vec<Node>;
    /// Every Edge that currently exists — same reasoning as `all_nodes`.
    fn all_edges(&self) -> Vec<Edge>;
    /// Removes a Node and every Edge touching it (in either
    /// direction). Returns `false` if `id` didn't exist. Deliberately
    /// has no opinion on `Role`-based cascading (e.g. an Entity's
    /// Occurrences) — that's a `rashomon:graph` domain convention, not
    /// something this generic store should bake in; see
    /// `rashomon-kernel`'s `delete_node` Host impl for that.
    fn delete_node(&mut self, id: &str) -> bool;
}

#[derive(Debug, Default)]
pub struct InMemoryGraphStore {
    nodes: HashMap<String, Node>,
    edges: HashMap<String, Edge>,
}

impl InMemoryGraphStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GraphStore for InMemoryGraphStore {
    fn create_node(&mut self, node_type: &str, role: Role, properties: HashMap<String, String>) -> Node {
        let node = Node {
            id: Uuid::new_v4().to_string(),
            node_type: node_type.to_string(),
            role,
            properties,
        };
        self.nodes.insert(node.id.clone(), node.clone());
        node
    }

    fn get_node(&self, id: &str) -> Option<Node> {
        self.nodes.get(id).cloned()
    }

    fn create_edge(&mut self, edge_type: &str, source: &str, target: &str, confidence: f32) -> Edge {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs();
        let edge = Edge {
            id: Uuid::new_v4().to_string(),
            edge_type: edge_type.to_string(),
            source: source.to_string(),
            target: target.to_string(),
            timestamp,
            confidence,
        };
        self.edges.insert(edge.id.clone(), edge.clone());
        edge
    }

    fn query_edges_from(&self, node_id: &str) -> Vec<Edge> {
        self.edges.values().filter(|e| e.source == node_id).cloned().collect()
    }

    fn query_edges_to(&self, node_id: &str) -> Vec<Edge> {
        self.edges.values().filter(|e| e.target == node_id).cloned().collect()
    }

    fn all_nodes(&self) -> Vec<Node> {
        self.nodes.values().cloned().collect()
    }

    fn all_edges(&self) -> Vec<Edge> {
        self.edges.values().cloned().collect()
    }

    fn delete_node(&mut self, id: &str) -> bool {
        if self.nodes.remove(id).is_none() {
            return false;
        }
        self.edges.retain(|_, e| e.source != id && e.target != id);
        true
    }
}

/// A `GraphStore` that survives a restart. A `petgraph::DiGraph` is the
/// live, queryable index — `query_edges_from`/`query_edges_to` are
/// adjacency lookups instead of `InMemoryGraphStore`'s full scan — and
/// every write is also committed to a `redb` file before returning, so
/// `open`-ing the same path again replays the same graph.
pub struct PersistentGraphStore {
    db: Database,
    graph: DiGraph<Node, Edge>,
    node_index: HashMap<String, NodeIndex>,
}

impl PersistentGraphStore {
    /// Opens (creating if needed) the `redb` file at `path` and replays
    /// any persisted Nodes and Edges into a fresh in-memory index.
    pub fn open(path: impl AsRef<Path>) -> Self {
        let db = Database::create(path).expect("failed to open redb database");

        let mut graph = DiGraph::new();
        let mut node_index = HashMap::new();

        let read_txn = db.begin_read().expect("failed to begin redb read transaction");

        if let Ok(table) = read_txn.open_table(NODES_TABLE) {
            for row in table.iter().expect("failed to iterate nodes table") {
                let (_, value) = row.expect("failed to read node row");
                let node: Node = serde_json::from_slice(value.value())
                    .expect("failed to deserialize persisted node");
                let idx = graph.add_node(node.clone());
                node_index.insert(node.id, idx);
            }
        }

        if let Ok(table) = read_txn.open_table(EDGES_TABLE) {
            for row in table.iter().expect("failed to iterate edges table") {
                let (_, value) = row.expect("failed to read edge row");
                let edge: Edge = serde_json::from_slice(value.value())
                    .expect("failed to deserialize persisted edge");
                let source = node_index[&edge.source];
                let target = node_index[&edge.target];
                graph.add_edge(source, target, edge);
            }
        }

        Self {
            db,
            graph,
            node_index,
        }
    }

    fn persist_node(&self, node: &Node) {
        let bytes = serde_json::to_vec(node).expect("failed to serialize node");
        let write_txn = self.db.begin_write().expect("failed to begin redb write transaction");
        {
            let mut table = write_txn.open_table(NODES_TABLE).expect("failed to open nodes table");
            table
                .insert(node.id.as_str(), bytes.as_slice())
                .expect("failed to persist node");
        }
        write_txn.commit().expect("failed to commit node write");
    }

    fn persist_edge(&self, edge: &Edge) {
        let bytes = serde_json::to_vec(edge).expect("failed to serialize edge");
        let write_txn = self.db.begin_write().expect("failed to begin redb write transaction");
        {
            let mut table = write_txn.open_table(EDGES_TABLE).expect("failed to open edges table");
            table
                .insert(edge.id.as_str(), bytes.as_slice())
                .expect("failed to persist edge");
        }
        write_txn.commit().expect("failed to commit edge write");
    }

    /// `edge_ids` must be every Edge touching `node_id` — leaving one
    /// behind would panic the *next* time this file is `open`-ed: edge
    /// replay looks up both endpoints' `NodeIndex` unconditionally
    /// (see `open`), and a dangling edge's endpoint wouldn't exist
    /// anymore.
    fn persist_delete_node(&self, node_id: &str, edge_ids: &[String]) {
        let write_txn = self.db.begin_write().expect("failed to begin redb write transaction");
        {
            let mut nodes_table = write_txn.open_table(NODES_TABLE).expect("failed to open nodes table");
            nodes_table.remove(node_id).expect("failed to delete persisted node");
            let mut edges_table = write_txn.open_table(EDGES_TABLE).expect("failed to open edges table");
            for edge_id in edge_ids {
                edges_table.remove(edge_id.as_str()).expect("failed to delete persisted edge");
            }
        }
        write_txn.commit().expect("failed to commit node deletion");
    }
}

impl GraphStore for PersistentGraphStore {
    fn create_node(&mut self, node_type: &str, role: Role, properties: HashMap<String, String>) -> Node {
        let node = Node {
            id: Uuid::new_v4().to_string(),
            node_type: node_type.to_string(),
            role,
            properties,
        };
        self.persist_node(&node);
        let idx = self.graph.add_node(node.clone());
        self.node_index.insert(node.id.clone(), idx);
        node
    }

    fn get_node(&self, id: &str) -> Option<Node> {
        let idx = *self.node_index.get(id)?;
        Some(self.graph[idx].clone())
    }

    /// Unlike `InMemoryGraphStore`, `source` and `target` must already
    /// exist as Nodes — petgraph needs their `NodeIndex` to place the
    /// edge, so an unknown id panics rather than being silently stored.
    fn create_edge(&mut self, edge_type: &str, source: &str, target: &str, confidence: f32) -> Edge {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs();
        let edge = Edge {
            id: Uuid::new_v4().to_string(),
            edge_type: edge_type.to_string(),
            source: source.to_string(),
            target: target.to_string(),
            timestamp,
            confidence,
        };
        self.persist_edge(&edge);

        let source_idx = *self
            .node_index
            .get(source)
            .expect("create_edge: unknown source node id");
        let target_idx = *self
            .node_index
            .get(target)
            .expect("create_edge: unknown target node id");
        self.graph.add_edge(source_idx, target_idx, edge.clone());

        edge
    }

    fn query_edges_from(&self, node_id: &str) -> Vec<Edge> {
        let Some(&idx) = self.node_index.get(node_id) else {
            return Vec::new();
        };
        self.graph
            .edges_directed(idx, Direction::Outgoing)
            .map(|e| e.weight().clone())
            .collect()
    }

    fn query_edges_to(&self, node_id: &str) -> Vec<Edge> {
        let Some(&idx) = self.node_index.get(node_id) else {
            return Vec::new();
        };
        self.graph
            .edges_directed(idx, Direction::Incoming)
            .map(|e| e.weight().clone())
            .collect()
    }

    fn all_nodes(&self) -> Vec<Node> {
        self.graph.node_weights().cloned().collect()
    }

    fn all_edges(&self) -> Vec<Edge> {
        self.graph.edge_weights().cloned().collect()
    }

    fn delete_node(&mut self, id: &str) -> bool {
        let Some(&idx) = self.node_index.get(id) else {
            return false;
        };

        let touching_edge_ids: Vec<String> = self
            .graph
            .edges_directed(idx, Direction::Outgoing)
            .chain(self.graph.edges_directed(idx, Direction::Incoming))
            .map(|e| e.weight().id.clone())
            .collect();

        self.graph.remove_node(idx);
        // `remove_node` moves the graph's *last* node into `idx`'s now-
        // vacant slot, invalidating whatever `NodeIndex` that node was
        // tracked under — rebuilding from scratch rather than patching
        // just that one entry, since this is a rare, not-hot-path
        // operation and a full rebuild can't get this subtlety wrong.
        self.node_index = self.graph.node_indices().map(|i| (self.graph[i].id.clone(), i)).collect();

        self.persist_delete_node(id, &touching_edge_ids);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get_node() {
        let mut store = InMemoryGraphStore::new();
        let node = store.create_node("rashomon:page", Role::Entity, HashMap::new());

        assert_eq!(store.get_node(&node.id), Some(node));
    }

    #[test]
    fn get_missing_node_returns_none() {
        let store = InMemoryGraphStore::new();
        assert_eq!(store.get_node("does-not-exist"), None);
    }

    #[test]
    fn create_edge_sets_fields() {
        let mut store = InMemoryGraphStore::new();
        let a = store.create_node("rashomon:page", Role::Entity, HashMap::new());
        let b = store.create_node("rashomon:page", Role::Occurrence, HashMap::new());

        let edge = store.create_edge("references", &a.id, &b.id, 0.9);

        assert_eq!(edge.source, a.id);
        assert_eq!(edge.target, b.id);
        assert_eq!(edge.edge_type, "references");
        assert_eq!(edge.confidence, 0.9);
    }

    #[test]
    fn query_edges_from_and_to() {
        let mut store = InMemoryGraphStore::new();
        let a = store.create_node("rashomon:page", Role::Entity, HashMap::new());
        let b = store.create_node("rashomon:page", Role::Entity, HashMap::new());
        let c = store.create_node("rashomon:page", Role::Entity, HashMap::new());

        let ab = store.create_edge("references", &a.id, &b.id, 1.0);
        let ac = store.create_edge("references", &a.id, &c.id, 1.0);
        let cb = store.create_edge("related-to", &c.id, &b.id, 1.0);

        let mut from_a: Vec<String> = store.query_edges_from(&a.id).into_iter().map(|e| e.id).collect();
        from_a.sort();
        let mut expected_from_a = vec![ab.id.clone(), ac.id.clone()];
        expected_from_a.sort();
        assert_eq!(from_a, expected_from_a);

        let to_b: Vec<String> = store.query_edges_to(&b.id).into_iter().map(|e| e.id).collect();
        assert_eq!(to_b.len(), 2);
        assert!(to_b.contains(&ab.id));
        assert!(to_b.contains(&cb.id));

        assert!(store.query_edges_from(&b.id).is_empty());
    }

    fn temp_db_path() -> tempfile::TempPath {
        tempfile::NamedTempFile::new()
            .expect("failed to create temp file")
            .into_temp_path()
    }

    #[test]
    fn persistent_create_and_get_node() {
        let path = temp_db_path();
        let mut store = PersistentGraphStore::open(&path);
        let node = store.create_node("rashomon:page", Role::Entity, HashMap::new());

        assert_eq!(store.get_node(&node.id), Some(node));
    }

    #[test]
    fn persistent_get_missing_node_returns_none() {
        let path = temp_db_path();
        let store = PersistentGraphStore::open(&path);
        assert_eq!(store.get_node("does-not-exist"), None);
    }

    #[test]
    fn persistent_create_edge_sets_fields() {
        let path = temp_db_path();
        let mut store = PersistentGraphStore::open(&path);
        let a = store.create_node("rashomon:page", Role::Entity, HashMap::new());
        let b = store.create_node("rashomon:page", Role::Occurrence, HashMap::new());

        let edge = store.create_edge("references", &a.id, &b.id, 0.9);

        assert_eq!(edge.source, a.id);
        assert_eq!(edge.target, b.id);
        assert_eq!(edge.edge_type, "references");
        assert_eq!(edge.confidence, 0.9);
    }

    #[test]
    fn persistent_query_edges_from_and_to() {
        let path = temp_db_path();
        let mut store = PersistentGraphStore::open(&path);
        let a = store.create_node("rashomon:page", Role::Entity, HashMap::new());
        let b = store.create_node("rashomon:page", Role::Entity, HashMap::new());
        let c = store.create_node("rashomon:page", Role::Entity, HashMap::new());

        let ab = store.create_edge("references", &a.id, &b.id, 1.0);
        let ac = store.create_edge("references", &a.id, &c.id, 1.0);
        let cb = store.create_edge("related-to", &c.id, &b.id, 1.0);

        let mut from_a: Vec<String> = store.query_edges_from(&a.id).into_iter().map(|e| e.id).collect();
        from_a.sort();
        let mut expected_from_a = vec![ab.id.clone(), ac.id.clone()];
        expected_from_a.sort();
        assert_eq!(from_a, expected_from_a);

        let to_b: Vec<String> = store.query_edges_to(&b.id).into_iter().map(|e| e.id).collect();
        assert_eq!(to_b.len(), 2);
        assert!(to_b.contains(&ab.id));
        assert!(to_b.contains(&cb.id));

        assert!(store.query_edges_from(&b.id).is_empty());
    }

    #[test]
    fn persistent_store_survives_reopen() {
        let path = temp_db_path();

        let (node_id, edge_id) = {
            let mut store = PersistentGraphStore::open(&path);
            let a = store.create_node("rashomon:page", Role::Entity, HashMap::new());
            let b = store.create_node("rashomon:page", Role::Occurrence, HashMap::new());
            let edge = store.create_edge("occurrence-of", &b.id, &a.id, 1.0);
            (a.id, edge.id)
        };
        // `store` is dropped here, closing the redb database — reopening
        // the same path should replay everything written above.

        let reopened = PersistentGraphStore::open(&path);
        assert!(reopened.get_node(&node_id).is_some());

        let edges_to_node: Vec<String> = reopened
            .query_edges_to(&node_id)
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(edges_to_node, vec![edge_id]);
    }

    #[test]
    fn delete_node_removes_touching_edges() {
        let mut store = InMemoryGraphStore::new();
        let a = store.create_node("rashomon:page", Role::Entity, HashMap::new());
        let b = store.create_node("rashomon:page", Role::Entity, HashMap::new());
        let c = store.create_node("rashomon:page", Role::Entity, HashMap::new());
        store.create_edge("references", &a.id, &b.id, 1.0);
        store.create_edge("references", &c.id, &a.id, 1.0);
        // Untouched by deleting `a` — should survive.
        let bc = store.create_edge("references", &b.id, &c.id, 1.0);

        assert!(store.delete_node(&a.id));
        assert_eq!(store.get_node(&a.id), None);
        assert!(store.query_edges_from(&a.id).is_empty());
        assert!(store.query_edges_to(&a.id).is_empty());
        assert_eq!(store.all_edges().into_iter().map(|e| e.id).collect::<Vec<_>>(), vec![bc.id]);

        assert!(!store.delete_node(&a.id), "deleting an already-deleted node should report false");
    }

    #[test]
    fn persistent_delete_node_survives_reopen() {
        let path = temp_db_path();

        let (a_id, b_id, bc_id) = {
            let mut store = PersistentGraphStore::open(&path);
            let a = store.create_node("rashomon:page", Role::Entity, HashMap::new());
            let b = store.create_node("rashomon:page", Role::Entity, HashMap::new());
            let c = store.create_node("rashomon:page", Role::Entity, HashMap::new());
            store.create_edge("references", &a.id, &b.id, 1.0);
            let bc = store.create_edge("references", &b.id, &c.id, 1.0);

            assert!(store.delete_node(&a.id));
            (a.id, b.id, bc.id)
        };
        // Reopening must not panic replaying an edge whose endpoint no
        // longer exists (the whole reason persisted edges touching a
        // deleted node have to be removed too, not just the node row).

        let reopened = PersistentGraphStore::open(&path);
        assert_eq!(reopened.get_node(&a_id), None);
        assert!(reopened.get_node(&b_id).is_some());
        assert_eq!(reopened.all_edges().into_iter().map(|e| e.id).collect::<Vec<_>>(), vec![bc_id]);
    }
}
