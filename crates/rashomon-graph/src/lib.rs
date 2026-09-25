//! Plain Rust graph types mirroring `wit/deps/graph.wit`, plus a
//! `GraphStore` trait and an in-memory implementation. No persistence,
//! no file I/O — see `rashomon-architecture.md` sections 1, 3, 4.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    pub kind: String,
    pub source: String,
    pub target: String,
    pub timestamp: u64,
    pub confidence: f32,
}

pub trait GraphStore {
    fn create_node(&mut self, node_type: &str, role: Role, properties: HashMap<String, String>) -> Node;
    fn get_node(&self, id: &str) -> Option<Node>;
    fn create_edge(&mut self, kind: &str, source: &str, target: &str, confidence: f32) -> Edge;
    fn query_edges_from(&self, node_id: &str) -> Vec<Edge>;
    fn query_edges_to(&self, node_id: &str) -> Vec<Edge>;
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

    fn create_edge(&mut self, kind: &str, source: &str, target: &str, confidence: f32) -> Edge {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs();
        let edge = Edge {
            id: Uuid::new_v4().to_string(),
            kind: kind.to_string(),
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
        assert_eq!(edge.kind, "references");
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
}
