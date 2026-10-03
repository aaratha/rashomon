#[allow(warnings)]
mod bindings;

use bindings::exports::rashomon::facet::contract::Guest;
use bindings::rashomon::graph::store::{list_edges, list_nodes};
use bindings::rashomon::graph::types::Role;

/// Vendored, not loaded from a CDN `<script src>` — this Facet is
/// core local tooling (a graph inspector), not something that should
/// stop working without a live internet connection just because of
/// how its diagram library happens to be fetched. Embedded into the
/// compiled Component itself via `include_str!`.
const VIS_NETWORK_JS: &str = include_str!("../vendor/vis-network.min.js");

struct Component;

impl Guest for Component {
    /// Static chrome only — [`PAGE`] fetches the actual graph via the
    /// same poll round trip `terminal-xterm`/`sidebar` already use.
    /// `node_id` is unused: this View shows the *whole* graph, not one
    /// node's neighborhood, so it isn't "about" any particular Entity
    /// the way other Facets' Views are.
    fn render(_node_id: String) -> String {
        // A simple marker + `.replace()`, not `format!` — `PAGE`'s own
        // inline JS is full of literal `{`/`}` that would all need
        // escaping to `{{`/`}}` to use `format!` safely, which isn't
        // worth the risk of a missed brace for one substitution.
        PAGE.replace("/*VIS_NETWORK_JS*/", VIS_NETWORK_JS)
    }

    fn handle_input(_event: String) -> Vec<String> {
        Vec::new()
    }

    /// Polled on a `setInterval`, same as `sidebar`'s tab list — a
    /// full re-serialized snapshot each time, not an incremental diff,
    /// since `list-nodes`/`list-edges` are cheap and the graph here is
    /// small enough that re-rendering the whole list is simpler than
    /// tracking what changed.
    fn poll_output() -> String {
        let mut lines = Vec::new();
        for node in list_nodes() {
            let role = match node.role {
                Role::Entity => "entity",
                Role::Occurrence => "occurrence",
            };
            let properties = node
                .properties
                .iter()
                .map(|p| format!("{}={}", p.key, p.value))
                .collect::<Vec<_>>()
                .join(";");
            lines.push(format!("NODE\t{}\t{}\t{}\t{}", node.id, node.node_type, role, properties));
        }
        for edge in list_edges() {
            lines.push(format!(
                "EDGE\t{}\t{}\t{}\t{}\t{}",
                edge.id, edge.edge_type, edge.source, edge.target, edge.confidence
            ));
        }
        lines.join("\n")
    }
}

/// This View's entire UI — ordinary HTML/CSS/JS, same as `sidebar`:
/// freely restylable by editing this Component, no native-layer
/// involvement at all. Two modes sharing one `poll-output` feed: List
/// (the original table view) and Diagram (`vis-network`, vendored
/// locally — see [`VIS_NETWORK_JS`] — not loaded from a CDN, which
/// gives zoom/pan/drag for free, no custom canvas code needed).
const PAGE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8" />
<script>/*VIS_NETWORK_JS*/</script>
<style>
  html, body { margin: 0; height: 100%; background: #1e1e1e; color: #eee; font-family: -apple-system, sans-serif; }
  body { display: flex; flex-direction: column; box-sizing: border-box; }
  #toolbar { display: flex; gap: 6px; padding: 8px 16px; border-bottom: 1px solid #333; flex: 0 0 auto; }
  #toolbar button { padding: 6px 12px; border: none; border-radius: 6px; background: #333; color: #eee; cursor: pointer; font-size: 12px; }
  #toolbar button.active { background: #0a84ff; }
  #list-view { padding: 16px; box-sizing: border-box; overflow: auto; flex: 1 1 auto; }
  #diagram-view { flex: 1 1 auto; position: relative; }
  #network { position: absolute; inset: 0; }
  h2 { font-size: 14px; text-transform: uppercase; letter-spacing: 0.05em; color: #888; margin: 20px 0 8px; }
  h2:first-child { margin-top: 0; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th { text-align: left; color: #888; font-weight: normal; padding: 4px 8px; border-bottom: 1px solid #333; }
  td { padding: 4px 8px; border-bottom: 1px solid #292929; font-family: ui-monospace, monospace; }
  .entity { color: #7ec8ff; }
  .occurrence { color: #ffcf7e; }
  .empty { color: #666; font-style: italic; padding: 4px 8px; }
</style>
</head>
<body>
<div id="toolbar">
  <button id="list-btn" class="active">List</button>
  <button id="diagram-btn">Diagram</button>
</div>
<div id="list-view">
  <h2>Nodes</h2>
  <table id="nodes"><thead><tr><th>id</th><th>type</th><th>role</th><th>properties</th></tr></thead><tbody></tbody></table>
  <h2>Edges</h2>
  <table id="edges"><thead><tr><th>type</th><th>source</th><th>target</th><th>confidence</th></tr></thead><tbody></tbody></table>
</div>
<div id="diagram-view" style="display:none;">
  <div id="network"></div>
</div>
<script>
  const windowId = window.__rashomonWindowId || '';

  const listBtn = document.getElementById('list-btn');
  const diagramBtn = document.getElementById('diagram-btn');
  const listView = document.getElementById('list-view');
  const diagramView = document.getElementById('diagram-view');

  function setMode(mode) {
    listBtn.classList.toggle('active', mode === 'list');
    diagramBtn.classList.toggle('active', mode === 'diagram');
    listView.style.display = mode === 'list' ? '' : 'none';
    diagramView.style.display = mode === 'diagram' ? '' : 'none';
    if (mode === 'diagram') network.redraw();
  }
  listBtn.onclick = function () { setMode('list'); };
  diagramBtn.onclick = function () { setMode('diagram'); };

  // Zoom (scroll wheel), pan (drag canvas), and node dragging all come
  // free from vis-network's defaults — no custom interaction code needed.
  const network = new vis.Network(
    document.getElementById('network'),
    { nodes: [], edges: [] },
    {
      nodes: { shape: 'dot', size: 10, font: { color: '#eee', size: 12 }, borderWidth: 1 },
      edges: {
        arrows: 'to',
        color: { color: '#555', highlight: '#0a84ff' },
        font: { color: '#aaa', size: 10, strokeWidth: 0 },
        smooth: { type: 'continuous' },
      },
      physics: { stabilization: { iterations: 100 } },
      interaction: { hover: true },
    }
  );

  function shorten(id) {
    return id.length > 12 ? id.slice(0, 8) + '…' : id;
  }

  function cell(text, title) {
    const td = document.createElement('td');
    td.textContent = text;
    td.title = title || text;
    return td;
  }

  let lastKey = '';
  function refresh() {
    window.cefQuery({
      request: windowId + ':__poll__',
      onSuccess: function (response) {
        if (response === lastKey) return;
        lastKey = response;

        const nodesBody = document.querySelector('#nodes tbody');
        const edgesBody = document.querySelector('#edges tbody');
        nodesBody.innerHTML = '';
        edgesBody.innerHTML = '';

        const visNodes = [];
        const visEdges = [];

        let nodeCount = 0;
        let edgeCount = 0;
        (response || '').split('\n').filter(Boolean).forEach(function (line) {
          const parts = line.split('\t');
          if (parts[0] === 'NODE') {
            nodeCount++;
            const [, id, nodeType, role, properties] = parts;
            const row = document.createElement('tr');
            row.appendChild(cell(shorten(id), id));
            row.appendChild(cell(nodeType));
            const roleCell = cell(role);
            roleCell.className = role;
            row.appendChild(roleCell);
            row.appendChild(cell(properties || ''));
            nodesBody.appendChild(row);

            visNodes.push({
              id: id,
              label: nodeType + '\n' + shorten(id),
              color: role === 'entity' ? '#7ec8ff' : '#ffcf7e',
              title: id,
            });
          } else if (parts[0] === 'EDGE') {
            edgeCount++;
            const [, , edgeType, source, target, confidence] = parts;
            const row = document.createElement('tr');
            row.appendChild(cell(edgeType));
            row.appendChild(cell(shorten(source), source));
            row.appendChild(cell(shorten(target), target));
            row.appendChild(cell(confidence));
            edgesBody.appendChild(row);

            visEdges.push({ from: source, to: target, label: edgeType });
          }
        });

        if (nodeCount === 0) {
          const row = document.createElement('tr');
          const td = document.createElement('td');
          td.colSpan = 4;
          td.className = 'empty';
          td.textContent = 'no nodes yet';
          row.appendChild(td);
          nodesBody.appendChild(row);
        }
        if (edgeCount === 0) {
          const row = document.createElement('tr');
          const td = document.createElement('td');
          td.colSpan = 4;
          td.className = 'empty';
          td.textContent = 'no edges yet';
          row.appendChild(td);
          edgesBody.appendChild(row);
        }

        // Replaces the whole dataset rather than diffing — only runs
        // when `poll-output` actually changed (guarded by `lastKey`
        // above), and doesn't reset zoom/pan (`setData` leaves the
        // current view alone unless `fit()` is called, which this
        // deliberately never does, so a user's pan/zoom survives
        // across refreshes).
        network.setData({ nodes: visNodes, edges: visEdges });
      },
      onFailure: function () {},
    });
  }

  setInterval(refresh, 1000);
  refresh();
</script>
</body>
</html>"#;

bindings::export!(Component with_types_in bindings);
