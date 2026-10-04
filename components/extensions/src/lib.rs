#[allow(warnings)]
mod bindings;

use std::cell::RefCell;
use std::collections::HashSet;

use bindings::exports::rashomon::facet::contract::Guest;
use bindings::rashomon::browser::control::create_context;
use bindings::rashomon::browser::types::BrowserContext;

// Persists across calls within this Component's one instantiation —
// see `terminal`'s identical `SESSION` pattern.
thread_local! {
    static CONTEXT: RefCell<Option<BrowserContext>> = const { RefCell::new(None) };
    /// Ids removed via the "Remove" button this session — `self.extensions`
    /// host-side (what `list-extensions` reflects) doesn't change until
    /// restart, so without this an extension would stay listed under
    /// "Installed" right after being removed, even though its entry in
    /// the persisted config is already gone. Reset naturally on the
    /// next restart, since this is just in-memory guest state.
    static REMOVED_THIS_SESSION: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

struct Component;

impl Guest for Component {
    /// On first call, creates the one `browser-context` this Facet
    /// uses for every later call. Static chrome only — [`PAGE`]
    /// fetches the actual lists via the same poll round trip
    /// `sidebar`/`graph-view` already use.
    fn render(_node_id: String) -> String {
        CONTEXT.with_borrow_mut(|ctx| {
            if ctx.is_none() {
                *ctx = Some(create_context());
            }
        });
        PAGE.to_string()
    }

    /// `add:<candidate-id>`, `remove:<extension-id>`, and
    /// `toggle-popup:<extension-id>` are this Component's own tiny
    /// protocol — the host doesn't know the shape, same as every
    /// other Facet's `handle-input`.
    fn handle_input(event: String) -> Vec<String> {
        CONTEXT.with_borrow(|ctx| {
            let Some(ctx) = ctx.as_ref() else { return };
            if let Some(id) = event.strip_prefix("add:") {
                let _ = ctx.add_extension(id);
                // The reverse of a removal: this id might have been
                // removed and re-added within the same session.
                REMOVED_THIS_SESSION.with_borrow_mut(|removed| {
                    removed.remove(id);
                });
            } else if let Some(id) = event.strip_prefix("remove:") {
                if ctx.remove_extension(id).is_ok() {
                    REMOVED_THIS_SESSION.with_borrow_mut(|removed| {
                        removed.insert(id.to_string());
                    });
                }
            } else if let Some(id) = event.strip_prefix("toggle-popup:") {
                let _ = ctx.toggle_extension_popup(id);
            }
        });
        Vec::new()
    }

    /// Polled on a `setInterval`, same as `sidebar`'s tab list — a
    /// full re-serialized snapshot of both lists each time, not an
    /// incremental diff.
    fn poll_output() -> String {
        CONTEXT.with_borrow(|ctx| {
            let Some(ctx) = ctx.as_ref() else { return String::new() };
            let mut lines = Vec::new();
            let removed = REMOVED_THIS_SESSION.with_borrow(|r| r.clone());
            for ext in ctx.list_extensions() {
                if removed.contains(&ext.id) {
                    continue;
                }
                lines.push(format!("ACTIVE\t{}\t{}\t{}", ext.id, ext.name, ext.popup_open));
            }
            for candidate in ctx.list_extension_candidates() {
                lines.push(format!(
                    "CANDIDATE\t{}\t{}\t{}",
                    candidate.id, candidate.name, candidate.source_browser
                ));
            }
            lines.join("\n")
        })
    }
}

/// This View's entire UI — ordinary HTML/CSS/JS, same as `sidebar`/
/// `graph-view`: freely restylable by editing this Component, no
/// native-layer involvement at all.
const PAGE: &str = r#"<!doctype html>
<html>
<head>
<title>Extensions</title>
<meta charset="utf-8" />
<style>
  html, body { margin: 0; height: 100%; background: #1e1e1e; color: #eee; font-family: -apple-system, sans-serif; }
  body { padding: 16px; box-sizing: border-box; overflow: auto; }
  h2 { font-size: 14px; text-transform: uppercase; letter-spacing: 0.05em; color: #888; margin: 20px 0 8px; }
  h2:first-child { margin-top: 0; }
  .row { display: flex; align-items: center; gap: 10px; padding: 8px; border-radius: 6px; background: #292929; margin-bottom: 6px; }
  .row .name { flex: 1; font-size: 13px; }
  .row .source { color: #888; font-size: 12px; }
  button { padding: 6px 10px; border: none; border-radius: 6px; background: #333; color: #eee; cursor: pointer; font-size: 12px; }
  button:hover { background: #444; }
  button.danger:hover { background: #a33; }
  .empty { color: #666; font-style: italic; font-size: 13px; }
  /* Fixed, not inline — the page scrolls under it rather than it
     scrolling away with the content, so it stays visible without
     needing to scroll back to the top to see it. */
  #restart-banner {
    display: none;
    position: fixed;
    top: 12px;
    left: 16px;
    right: 16px;
    z-index: 10;
    background: #0a84ff;
    color: #fff;
    padding: 10px 14px;
    border-radius: 6px;
    font-size: 13px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
  }
  /* Reserves space so fixed-position banner doesn't overlap the
     first heading once it appears. */
  body.restart-pending { padding-top: 56px; }
  /* A separate, transient confirmation — unlike the restart banner,
     this isn't meant to stay up; it fades in/out on its own to
     confirm the specific action just taken. */
  #toast {
    position: fixed;
    bottom: 16px;
    left: 16px;
    right: 16px;
    z-index: 10;
    background: #333;
    color: #eee;
    padding: 10px 14px;
    border-radius: 6px;
    font-size: 13px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
    opacity: 0;
    transform: translateY(8px);
    transition: opacity 0.2s ease, transform 0.2s ease;
    pointer-events: none;
  }
  #toast.visible { opacity: 1; transform: translateY(0); }
</style>
</head>
<body>
<div id="restart-banner">Restart rashomon to apply extension changes.</div>
<div id="toast"></div>
<h2>Installed</h2>
<div id="active"></div>
<h2>Found in other browsers</h2>
<div id="candidates"></div>
<script>
  const windowId = window.__rashomonWindowId || '';

  function send(request) {
    window.cefQuery({
      request: windowId + ':' + request,
      onSuccess: function () {},
      onFailure: function () {},
    });
  }

  function showRestartBanner() {
    document.getElementById('restart-banner').style.display = 'block';
    document.body.classList.add('restart-pending');
  }

  let toastTimer = null;
  function showToast(text) {
    const toast = document.getElementById('toast');
    toast.textContent = text;
    toast.classList.add('visible');
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(function () {
      toast.classList.remove('visible');
    }, 3000);
  }

  function row(children) {
    const div = document.createElement('div');
    div.className = 'row';
    children.forEach(function (c) { div.appendChild(c); });
    return div;
  }

  function label(text, className) {
    const span = document.createElement('span');
    span.textContent = text;
    if (className) span.className = className;
    return span;
  }

  function button(text, danger, onClick) {
    const btn = document.createElement('button');
    btn.textContent = text;
    if (danger) btn.className = 'danger';
    btn.onclick = onClick;
    return btn;
  }

  let lastKey = '';
  function refresh() {
    window.cefQuery({
      request: windowId + ':__poll__',
      onSuccess: function (response) {
        if (response === lastKey) return;
        lastKey = response;

        const activeContainer = document.getElementById('active');
        const candidatesContainer = document.getElementById('candidates');
        activeContainer.innerHTML = '';
        candidatesContainer.innerHTML = '';

        let activeCount = 0;
        let candidateCount = 0;
        (response || '').split('\n').filter(Boolean).forEach(function (line) {
          const parts = line.split('\t');
          if (parts[0] === 'ACTIVE') {
            activeCount++;
            const [, id, name, popupOpen] = parts;
            activeContainer.appendChild(row([
              label(name, 'name'),
              button(popupOpen === 'true' ? 'Close Popup' : 'Open Popup', false, function () {
                send('toggle-popup:' + id);
              }),
              button('Remove', true, function () {
                send('remove:' + id);
                showRestartBanner();
                showToast('Removed ' + name + ' — it will stop loading after restart.');
              }),
            ]));
          } else if (parts[0] === 'CANDIDATE') {
            candidateCount++;
            const [, id, name, sourceBrowser] = parts;
            candidatesContainer.appendChild(row([
              label(name, 'name'),
              label('found in ' + sourceBrowser, 'source'),
              button('Add', false, function () {
                send('add:' + id);
                showRestartBanner();
              }),
            ]));
          }
        });

        if (activeCount === 0) {
          activeContainer.appendChild(label('no extensions installed', 'empty'));
        }
        if (candidateCount === 0) {
          candidatesContainer.appendChild(label('nothing new found', 'empty'));
        }
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
