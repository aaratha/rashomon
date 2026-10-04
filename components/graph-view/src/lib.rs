#[allow(warnings)]
mod bindings;

use bindings::exports::rashomon::facet::contract::Guest;
use bindings::rashomon::graph::store::{delete_node, list_edges, list_nodes};
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

    /// The page's own tiny protocol, same pattern as `sidebar`'s
    /// `switch-tab:<id>` — `"delete:<node-id>"` deletes that Node
    /// (cascading to its Occurrences if it's an Entity — see
    /// `delete-node`'s doc comment). The confirmation prompt (and the
    /// decision to allow deleting from either the list or the diagram
    /// view) lives entirely in [`PAGE`]'s own JS; by the time this
    /// runs, the user has already confirmed.
    fn handle_input(event: String) -> Vec<String> {
        if let Some(id) = event.strip_prefix("delete:") {
            delete_node(id);
        }
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
<title>Graph View</title>
<meta charset="utf-8" />
<script>/*VIS_NETWORK_JS*/</script>
<style>
  html, body { margin: 0; height: 100%; background: #1e1e1e; color: #eee; font-family: -apple-system, sans-serif; }
  body { display: flex; flex-direction: column; box-sizing: border-box; }
  #toolbar { display: flex; gap: 6px; padding: 8px 16px; border-bottom: 1px solid #333; flex: 0 0 auto; align-items: center; flex-wrap: wrap; }
  #toolbar button { padding: 6px 12px; border: none; border-radius: 6px; background: #333; color: #eee; cursor: pointer; font-size: 12px; }
  #toolbar button.active { background: #0a84ff; }
  #filters { display: flex; gap: 14px; margin-left: 16px; flex-wrap: wrap; }
  .filter-group { display: flex; gap: 10px; align-items: center; font-size: 12px; color: #aaa; }
  .filter-group .filter-group-label { color: #666; text-transform: uppercase; letter-spacing: 0.05em; font-size: 11px; }
  .filter-group label { display: flex; align-items: center; gap: 4px; cursor: pointer; user-select: none; }
  .filter-group input { cursor: pointer; }
  #list-view { padding: 16px; box-sizing: border-box; overflow: auto; flex: 1 1 auto; }
  #diagram-view { flex: 1 1 auto; position: relative; }
  #network { position: absolute; inset: 0; }
  h2 { font-size: 14px; text-transform: uppercase; letter-spacing: 0.05em; color: #888; margin: 20px 0 8px; }
  h2:first-child { margin-top: 0; }
  /* `table-layout: fixed` (plus the explicit per-column widths below)
     is what makes the `overflow`/`text-overflow` truncation on `td`
     actually take effect — with the default `auto` layout, a column
     just grows to fit its longest cell (e.g. a `rashomon:page`
     Entity's full, untruncated `url` property) instead of clipping
     it, which is what was blowing this table out past the window's
     own width. The full value is still one hover away (`cell()` sets
     `title` to it unconditionally). */
  table { width: 100%; border-collapse: collapse; font-size: 13px; table-layout: fixed; }
  th { text-align: left; color: #888; font-weight: normal; padding: 4px 8px; border-bottom: 1px solid #333; }
  td { padding: 4px 8px; border-bottom: 1px solid #292929; font-family: ui-monospace, monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  #nodes th:nth-child(1), #nodes td:nth-child(1) { width: 110px; }
  #nodes th:nth-child(2), #nodes td:nth-child(2) { width: 160px; }
  #nodes th:nth-child(3), #nodes td:nth-child(3) { width: 90px; }
  #nodes th:nth-child(5), #nodes td:nth-child(5) { width: 40px; }
  #edges th:nth-child(1), #edges td:nth-child(1) { width: 140px; }
  #edges th:nth-child(4), #edges td:nth-child(4) { width: 90px; }
  .entity { color: #7ec8ff; }
  .occurrence { color: #ffcf7e; }
  .empty { color: #666; font-style: italic; padding: 4px 8px; }
  .delete-btn { padding: 2px 8px; border: none; border-radius: 4px; background: transparent; color: #888; cursor: pointer; font-size: 12px; }
  .delete-btn:hover { background: #a33; color: #eee; }
  /* `window.confirm()` is a no-op in this app — `wry`'s WKWebView has
     no UI delegate wired up for JS dialog panels, so it never shows
     anything and just returns `false` immediately. This is a from-
     scratch replacement, not a styling choice. */
  #confirm-overlay { display: none; position: fixed; inset: 0; background: rgba(0,0,0,0.5); align-items: center; justify-content: center; z-index: 1000; }
  #confirm-box { background: #2a2a2a; border: 1px solid #444; border-radius: 8px; padding: 16px 20px; max-width: 320px; box-shadow: 0 4px 20px rgba(0,0,0,0.4); }
  #confirm-message { margin: 0 0 16px; font-size: 13px; color: #eee; }
  #confirm-actions { display: flex; justify-content: flex-end; gap: 8px; }
  #confirm-actions button { padding: 6px 12px; border: none; border-radius: 6px; cursor: pointer; font-size: 12px; }
  #confirm-cancel-btn { background: #333; color: #eee; }
  #confirm-cancel-btn:hover { background: #444; }
  #confirm-ok-btn { background: #a33; color: #eee; }
  #confirm-ok-btn:hover { background: #c44; }
</style>
</head>
<body>
<div id="toolbar">
  <button id="list-btn" class="active">List</button>
  <button id="diagram-btn">Diagram</button>
  <div id="filters">
    <span class="filter-group"><span class="filter-group-label">Type</span><span id="type-filters"></span></span>
    <span class="filter-group"><span class="filter-group-label">Role</span><span id="role-filters"></span></span>
  </div>
</div>
<div id="list-view">
  <h2>Nodes</h2>
  <table id="nodes"><thead><tr><th>id</th><th>type</th><th>role</th><th>properties</th><th></th></tr></thead><tbody></tbody></table>
  <h2>Edges</h2>
  <table id="edges"><thead><tr><th>type</th><th>source</th><th>target</th><th>confidence</th></tr></thead><tbody></tbody></table>
</div>
<div id="diagram-view" style="display:none;">
  <div id="network"></div>
</div>
<div id="confirm-overlay">
  <div id="confirm-box">
    <p id="confirm-message"></p>
    <div id="confirm-actions">
      <button id="confirm-cancel-btn">Cancel</button>
      <button id="confirm-ok-btn">Delete</button>
    </div>
  </div>
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

  // Pan (drag canvas) and node dragging come free from vis-network's
  // defaults. Scroll-wheel zoom (`zoomView`) is disabled here and
  // reimplemented below instead, alongside trackpad two-finger pan/
  // pinch — see `networkEl`'s `wheel` listener for why both need to
  // share one handler rather than letting vis-network's own default
  // coexist with it.
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
      interaction: { hover: true, zoomView: false },
    }
  );

  // Trackpad vs. mouse can't be told apart directly from a `wheel`
  // event (there's no device-type field on it) — this is the same
  // heuristic Mapbox GL JS/OpenLayers use: a real mouse wheel's
  // `wheelDeltaY` is always a multiple of 120 (the historical "one
  // notch" unit everything still reports in, even on modern high-res
  // mice); a trackpad's continuous swipe essentially never lands
  // exactly on a multiple of 120 by chance. WebKit (and so this
  // WKWebView-hosted page) also sets `ctrlKey` on the synthesized
  // `wheel` event it generates for an actual pinch gesture — a
  // separate, unambiguous signal checked first, below.
  function isTrackpadWheelEvent(e) {
    return !e.wheelDeltaY || e.wheelDeltaY % 120 !== 0;
  }

  // `network.moveTo({ scale, position })` treats `position` as the
  // canvas point that should end up centered in the viewport — *not*
  // "zoom while keeping this point fixed under the cursor" the way a
  // real cursor-centered zoom needs. Keeping `pointer` fixed on screen
  // means the view's center has to move too, by exactly the amount
  // `pointer` would otherwise have drifted from the zoom alone: at the
  // old scale `s0` and view center `c0`, `pointer` sits
  // `(pointer - c0) * s0` screen-pixels from center; solving for the
  // new center `c1` that keeps that same screen offset at the new
  // scale `s1` gives `c1 = pointer + (c0 - pointer) * (s0 / s1)`.
  function zoomAroundPointer(pointer, newScale) {
    const oldScale = network.getScale();
    const center = network.getViewPosition();
    const ratio = oldScale / newScale;
    network.moveTo({
      scale: newScale,
      position: {
        x: pointer.x + (center.x - pointer.x) * ratio,
        y: pointer.y + (center.y - pointer.y) * ratio,
      },
      animation: false,
    });
  }

  const networkEl = document.getElementById('network');
  networkEl.addEventListener(
    'wheel',
    function (e) {
      e.preventDefault();
      const rect = networkEl.getBoundingClientRect();
      const pointer = network.DOMtoCanvas({ x: e.clientX - rect.left, y: e.clientY - rect.top });

      if (e.ctrlKey) {
        // Trackpad pinch — `e.deltaY` is WebKit's own synthesized
        // zoom amount for this gesture, negative when pinching open
        // (zoom in), positive when pinching closed (zoom out).
        zoomAroundPointer(pointer, network.getScale() * Math.exp(-e.deltaY * 0.01));
      } else if (isTrackpadWheelEvent(e)) {
        // Trackpad two-finger pan — moves the view in the same
        // direction as the swipe (content follows the fingers), the
        // same "natural scrolling" convention macOS already uses
        // everywhere else. Divided by the current scale so a swipe
        // covers the same *screen* distance at any zoom level.
        const scale = network.getScale();
        const pos = network.getViewPosition();
        network.moveTo({
          position: { x: pos.x + e.deltaX / scale, y: pos.y + e.deltaY / scale },
          animation: false,
        });
      } else {
        // A real mouse's scroll wheel — unchanged from vis-network's
        // own previous default `zoomView` behavior: each notch zooms
        // in/out by a fixed step, centered on the cursor.
        const direction = e.deltaY < 0 ? 1 : -1;
        zoomAroundPointer(pointer, network.getScale() * Math.exp(direction * 0.1));
      }
    },
    { passive: false }
  );

  // Diagram-mode deletion: right-click a node (vis-network's own
  // 'oncontext' event, not a raw listener — it already resolves which
  // node, if any, is under the pointer) or select one and press
  // Delete/Backspace. `allNodes` (declared further below, but already
  // populated by the time either of these ever actually fires — both
  // are callbacks, not run during this script's own top-to-bottom
  // execution) is where the Role used for `deleteNode`'s confirmation
  // message comes from; vis-network's own dataset doesn't carry it.
  function findNodeRole(id) {
    const node = allNodes.find(function (n) { return n.id === id; });
    return node ? node.role : undefined;
  }

  network.on('oncontext', function (params) {
    params.event.preventDefault();
    const nodeId = network.getNodeAt(params.pointer.DOM);
    if (nodeId === undefined) return;
    network.selectNodes([nodeId]);
    deleteNode(nodeId, findNodeRole(nodeId));
  });

  document.addEventListener('keydown', function (e) {
    if (diagramView.style.display === 'none') return;
    if (e.key !== 'Delete' && e.key !== 'Backspace') return;
    const selected = network.getSelectedNodes();
    if (selected.length === 0) return;
    e.preventDefault();
    deleteNode(selected[0], findNodeRole(selected[0]));
  });

  function shorten(id) {
    return id.length > 12 ? id.slice(0, 8) + '…' : id;
  }

  // Shared by both the List view's delete button and the Diagram
  // view's right-click/Delete-key handlers below — one confirmation +
  // request path regardless of which view a Node is deleted from.
  // The cascade-to-occurrences behavior itself is entirely host-side
  // (see `delete-node`'s doc comment) — this just warns about it
  // first, since there's no undo.
  // Stand-in for `window.confirm` (see the `#confirm-overlay` CSS
  // comment for why that doesn't work here) — resolves `true`/`false`
  // the same way, just via a real in-page element instead of a native
  // panel `wry` never shows.
  let resolveConfirm = null;
  const confirmOverlay = document.getElementById('confirm-overlay');
  function showConfirm(message) {
    document.getElementById('confirm-message').textContent = message;
    confirmOverlay.style.display = 'flex';
    return new Promise(function (resolve) { resolveConfirm = resolve; });
  }
  document.getElementById('confirm-cancel-btn').onclick = function () {
    confirmOverlay.style.display = 'none';
    if (resolveConfirm) resolveConfirm(false);
  };
  document.getElementById('confirm-ok-btn').onclick = function () {
    confirmOverlay.style.display = 'none';
    if (resolveConfirm) resolveConfirm(true);
  };

  function deleteNode(id, role) {
    const message = role === 'entity'
      ? 'Delete this entity and all of its occurrences? This cannot be undone.'
      : 'Delete this occurrence? This cannot be undone.';
    showConfirm(message).then(function (confirmed) {
      if (!confirmed) return;
      window.cefQuery({
        request: windowId + ':delete:' + id,
        onSuccess: function () {},
        onFailure: function () {},
      });
    });
  }

  function cell(text, title) {
    const td = document.createElement('td');
    td.textContent = text;
    td.title = title || text;
    return td;
  }

  // Full parsed snapshot from the last poll — kept around (not just
  // the filtered subset) so toggling a filter checkbox can re-render
  // immediately without waiting on the next poll round trip.
  let allNodes = [];
  let allEdges = [];

  // Which type/role values a user has explicitly unchecked. Absent
  // from these sets means visible — including types/roles not
  // discovered yet, so a brand new node type shows up checked by
  // default rather than hidden until the user notices and opts in.
  const hiddenTypes = new Set();
  const hiddenRoles = new Set();
  // What `buildFilterGroup` last rendered checkboxes for — only
  // rebuilt (which would otherwise junk the user's mid-click state)
  // when the actual set of known values changes.
  let knownTypesKey = '';
  let knownRolesKey = '';

  function buildFilterGroup(container, values, hidden, onChange) {
    container.innerHTML = '';
    values.forEach(function (value) {
      const labelEl = document.createElement('label');
      const checkbox = document.createElement('input');
      checkbox.type = 'checkbox';
      checkbox.checked = !hidden.has(value);
      checkbox.onchange = function () {
        if (checkbox.checked) hidden.delete(value); else hidden.add(value);
        onChange();
      };
      labelEl.appendChild(checkbox);
      labelEl.appendChild(document.createTextNode(value));
      container.appendChild(labelEl);
    });
  }

  // `properties` arrives as a raw `key=value;key=value` string (see
  // `poll_output` on the host side) — never parsed into a real object
  // until now, since the List view just displays it verbatim. Values
  // may themselves contain `=` (most property keys used so far are
  // short, fixed names like `url`/`title`, never containing `=`
  // themselves, so splitting on the *first* `=` only is enough).
  function parseProperties(raw) {
    const result = {};
    (raw || '').split(';').filter(Boolean).forEach(function (pair) {
      const i = pair.indexOf('=');
      if (i === -1) return;
      result[pair.slice(0, i)] = pair.slice(i + 1);
    });
    return result;
  }

  function truncate(text, maxLength) {
    return text.length > maxLength ? text.slice(0, maxLength - 1) + '…' : text;
  }

  // A real page URL (especially a search results page, or anything
  // else with query-string tracking params) can run to hundreds of
  // characters — nowhere near usable as a diagram label. Dropping the
  // query string/hash and keeping just host+path covers the vast
  // majority of what actually distinguishes one page from another;
  // the *full* url is still one hover away (see `visNodes.push`'s
  // `title` field below), this is just the label text.
  function shortenUrl(url) {
    let host = url;
    let path = '';
    try {
      const parsed = new URL(url);
      host = parsed.hostname;
      path = parsed.pathname === '/' ? '' : parsed.pathname;
    } catch (e) {
      // Not a real absolute URL (shouldn't happen for a `rashomon:page`
      // Entity, but this is display code, not worth a thrown error
      // over) — fall through to plain truncation of the whole string.
      return truncate(url, 40);
    }
    return truncate(host + path, 40);
  }

  // Tried in order — whichever is present first wins. `url` covers
  // `rashomon:page` Entities (see `rashomon-kernel`'s
  // `record_page_visit`); `title`/`name` are here for any future
  // Entity type that sets one, so this doesn't need revisiting every
  // time a new kind of labeled Entity shows up.
  const LABEL_PROPERTY_PRIORITY = ['url', 'title', 'name'];

  function entityLabel(node) {
    const props = parseProperties(node.properties);
    if (props.url) return shortenUrl(props.url);
    for (let i = 1; i < LABEL_PROPERTY_PRIORITY.length; i++) {
      const value = props[LABEL_PROPERTY_PRIORITY[i]];
      if (value) return truncate(value, 40);
    }
    return node.nodeType + '\n' + shorten(node.id);
  }

  // An Occurrence's own properties are rarely what identifies it to a
  // human — what matters is *which Entity it's an occurrence of* (see
  // the module doc comment's `occurrence-of` convention: the edge
  // always points *from* the Occurrence *to* the Entity). Labeled as
  // that Entity's own label, prefixed with `→`, rather than
  // `nodeType`/id like an Entity gets — "→google.com" reads far better
  // than "rashomon:page-visit\nab12cd34…". Falls back to the plain
  // `nodeType`/id label if the Occurrence has no `occurrence-of` edge
  // at all, or its target isn't in `allNodes` (both unexpected, but
  // not worth a crash over).
  function occurrenceLabel(node) {
    const edge = allEdges.find(function (e) {
      return e.edgeType === 'occurrence-of' && e.source === node.id;
    });
    const entity = edge && allNodes.find(function (n) { return n.id === edge.target; });
    if (!entity) return node.nodeType + '\n' + shorten(node.id);
    return '→ ' + entityLabel(entity);
  }

  function nodeLabel(node) {
    return node.role === 'entity' ? entityLabel(node) : occurrenceLabel(node);
  }

  function emptyRow(colSpan, text) {
    const row = document.createElement('tr');
    const td = document.createElement('td');
    td.colSpan = colSpan;
    td.className = 'empty';
    td.textContent = text;
    row.appendChild(td);
    return row;
  }

  // Re-renders the table + diagram from `allNodes`/`allEdges` filtered
  // by the current checkbox state — called after every poll and after
  // every filter checkbox change, so the two stay in sync without
  // re-fetching anything.
  function render() {
    const visibleNodes = allNodes.filter(function (n) {
      return !hiddenTypes.has(n.nodeType) && !hiddenRoles.has(n.role);
    });
    const visibleIds = new Set(visibleNodes.map(function (n) { return n.id; }));
    // An edge is only shown if both endpoints survived the filter —
    // an edge dangling to a hidden node would be more confusing than
    // useful.
    const visibleEdges = allEdges.filter(function (e) {
      return visibleIds.has(e.source) && visibleIds.has(e.target);
    });

    const nodesBody = document.querySelector('#nodes tbody');
    const edgesBody = document.querySelector('#edges tbody');
    nodesBody.innerHTML = '';
    edgesBody.innerHTML = '';

    const visNodes = [];
    const visEdges = [];

    visibleNodes.forEach(function (n) {
      const row = document.createElement('tr');
      row.appendChild(cell(shorten(n.id), n.id));
      row.appendChild(cell(n.nodeType));
      const roleCell = cell(n.role);
      roleCell.className = n.role;
      row.appendChild(roleCell);
      row.appendChild(cell(n.properties || ''));
      const actionsCell = document.createElement('td');
      const deleteBtn = document.createElement('button');
      deleteBtn.className = 'delete-btn';
      deleteBtn.textContent = '✕';
      deleteBtn.title = 'Delete';
      deleteBtn.onclick = function () { deleteNode(n.id, n.role); };
      actionsCell.appendChild(deleteBtn);
      row.appendChild(actionsCell);
      nodesBody.appendChild(row);

      // The hover tooltip carries the *untruncated* url (falling back
      // to the id) — `nodeLabel`'s text is deliberately shortened for
      // display, so this is the only place the full value is still
      // reachable without switching to the List view.
      const fullUrl = parseProperties(n.properties).url;
      visNodes.push({
        id: n.id,
        label: nodeLabel(n),
        color: n.role === 'entity' ? '#7ec8ff' : '#ffcf7e',
        title: fullUrl ? fullUrl + '\n' + n.id : n.id,
      });
    });

    visibleEdges.forEach(function (e) {
      const row = document.createElement('tr');
      row.appendChild(cell(e.edgeType));
      row.appendChild(cell(shorten(e.source), e.source));
      row.appendChild(cell(shorten(e.target), e.target));
      row.appendChild(cell(e.confidence));
      edgesBody.appendChild(row);

      visEdges.push({ from: e.source, to: e.target, label: e.edgeType });
    });

    if (visibleNodes.length === 0) {
      nodesBody.appendChild(emptyRow(5, allNodes.length === 0 ? 'no nodes yet' : 'no nodes match the filter'));
    }
    if (visibleEdges.length === 0) {
      edgesBody.appendChild(emptyRow(4, allEdges.length === 0 ? 'no edges yet' : 'no edges match the filter'));
    }

    // Replaces the whole dataset rather than diffing, but doesn't
    // reset zoom/pan (`setData` leaves the current view alone unless
    // `fit()` is called, which this deliberately never does, so a
    // user's pan/zoom survives across refreshes and filter changes).
    network.setData({ nodes: visNodes, edges: visEdges });
  }

  let lastKey = '';
  function refresh() {
    window.cefQuery({
      request: windowId + ':__poll__',
      onSuccess: function (response) {
        if (response === lastKey) return;
        lastKey = response;

        const nodes = [];
        const edges = [];
        (response || '').split('\n').filter(Boolean).forEach(function (line) {
          const parts = line.split('\t');
          if (parts[0] === 'NODE') {
            const [, id, nodeType, role, properties] = parts;
            nodes.push({ id: id, nodeType: nodeType, role: role, properties: properties || '' });
          } else if (parts[0] === 'EDGE') {
            const [, , edgeType, source, target, confidence] = parts;
            edges.push({ edgeType: edgeType, source: source, target: target, confidence: confidence });
          }
        });
        allNodes = nodes;
        allEdges = edges;

        const types = Array.from(new Set(nodes.map(function (n) { return n.nodeType; }))).sort();
        const roles = Array.from(new Set(nodes.map(function (n) { return n.role; }))).sort();
        const typesKey = types.join(',');
        const rolesKey = roles.join(',');
        if (typesKey !== knownTypesKey) {
          knownTypesKey = typesKey;
          buildFilterGroup(document.getElementById('type-filters'), types, hiddenTypes, render);
        }
        if (rolesKey !== knownRolesKey) {
          knownRolesKey = rolesKey;
          buildFilterGroup(document.getElementById('role-filters'), roles, hiddenRoles, render);
        }

        render();
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
