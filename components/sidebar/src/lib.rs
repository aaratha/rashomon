#[allow(warnings)]
mod bindings;

use std::cell::RefCell;

use bindings::exports::rashomon::facet::contract::Guest;
use bindings::rashomon::browser::control::create_context;
use bindings::rashomon::browser::types::BrowserContext;

// Persists across calls within this Component's one instantiation —
// see `terminal`'s identical `SESSION` pattern. The sidebar is opened
// once (`Kernel::open_sidebar`), never mirrored, so there's exactly
// one of these for the process's lifetime.
thread_local! {
    static CONTEXT: RefCell<Option<BrowserContext>> = const { RefCell::new(None) };
}

struct Component;

impl Guest for Component {
    /// On first call, creates the one `browser-context` this Facet
    /// uses for every later `handle-input`/`poll-output` call. Returns
    /// the sidebar's entire UI — this is the thing the user asked to
    /// be fully author-modifiable, not baked into the native host
    /// layer: everything here (styling, tab-list rendering, the
    /// popup-toggle button) is this Component's own HTML/CSS/JS, with
    /// no special host-side cooperation beyond the same
    /// `window.cefQuery` round trip every Facet already gets.
    fn render(_node_id: String) -> String {
        CONTEXT.with_borrow_mut(|ctx| {
            if ctx.is_none() {
                *ctx = Some(create_context());
            }
        });
        PAGE.to_string()
    }

    /// `event` has already had its `<window-id>:` routing prefix
    /// stripped by the host, same as every other Facet —
    /// `toggle-popup:<extension-id>` and `switch-tab:<id>` are this
    /// Component's own tiny protocol, not something the host knows
    /// the shape of.
    fn handle_input(event: String) -> Vec<String> {
        CONTEXT.with_borrow(|ctx| {
            let Some(ctx) = ctx.as_ref() else { return };
            if let Some(extension_id) = event.strip_prefix("toggle-popup:") {
                let _ = ctx.toggle_extension_popup(extension_id);
            } else if let Some(tab_id) = event.strip_prefix("switch-tab:") {
                let _ = ctx.switch_to_tab(tab_id);
            }
        });
        Vec::new()
    }

    /// Polled on a `setInterval`, same as `terminal-xterm`'s live
    /// output — returns both the extension icon row and the tab list
    /// as one `\n`-joined feed, each line tagged `EXT`/`TAB` (same
    /// convention `graph-view`'s `NODE`/`EDGE` poll format uses) since
    /// there's no other way to carry two differently-shaped lists back
    /// over one plain-string channel. No incremental diffing needed —
    /// both `list-extensions`/`list-tabs` are cheap, and the page
    /// itself only re-renders when the joined result actually changes.
    fn poll_output() -> String {
        CONTEXT.with_borrow(|ctx| {
            let Some(ctx) = ctx.as_ref() else { return String::new() };
            let mut lines: Vec<String> = ctx
                .list_extensions()
                .into_iter()
                .map(|ext| {
                    format!(
                        "EXT\t{}\t{}\t{}\t{}",
                        ext.id,
                        ext.name,
                        ext.icon.unwrap_or_default(),
                        ext.popup_open
                    )
                })
                .collect();
            lines.extend(ctx.list_tabs().into_iter().map(|tab| format!("TAB\t{}\t{}", tab.id, tab.title)));
            lines.join("\n")
        })
    }
}

/// The sidebar's entire UI — ordinary HTML/CSS/JS, freely restylable
/// or rearrangeable by editing this Component, not the host's native
/// windowing code (see `rashomon-kernel`'s `open_browser_switcher_window`,
/// which only ever reserves a fixed-width slot for whatever this
/// renders — it has no idea what's inside).
const PAGE: &str = r#"<!doctype html>
<html>
<head>
<title>Sidebar</title>
<meta charset="utf-8" />
<style>
  /* Translucent, not opaque — experimental test of whether the native
     Liquid Glass layer behind this page (see rashomon-kernel's
     `mac::apply_liquid_glass_background`) actually shows through a
     windowed (non-OSR) `BrowserView`'s content at all. */
  html, body { margin: 0; height: 100%; background: rgba(30, 30, 30, 0.55); color: #eee; font-family: -apple-system, sans-serif; }
  /* padding-top leaves room for the host's native traffic-light
     buttons, drawn over this page's top-left corner — without it the
     first button would sit right under them. */
  #sidebar { display: flex; flex-direction: column; padding: 8px; padding-top: 36px; gap: 6px; box-sizing: border-box; height: 100vh; }
  button {
    padding: 8px 10px;
    border: none;
    border-radius: 6px;
    /* Invisible at rest — only `:hover` below gives it any fill, so
       the sidebar reads as a flat list of labels sitting directly on
       the glass until you actually point at one. */
    background: transparent;
    color: #eee;
    cursor: pointer;
    text-align: left;
    font-size: 13px;
    transition: background 0.15s ease;
    /* Tab labels are currently the tab's raw URL (see
       rashomon-kernel's HostBrowserContext::list_tabs) — for
       terminal-xterm tabs that's a `data:text/html;base64,...` URI,
       thousands of characters long, so this needs to truncate rather
       than overflow the sidebar's fixed width. */
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    display: block;
  }
  button:hover { background: rgba(255, 255, 255, 0.16); }
  #tabs { display: flex; flex-direction: column; gap: 4px; margin-top: 8px; }
  #extensions { display: flex; flex-direction: row; gap: 4px; flex-wrap: wrap; }
  /* Square icon buttons, not the full-width label rows tabs use —
     same transparent-until-hover treatment either way (inherited from
     the plain `button` rule above), just a different shape/padding. */
  .ext-btn { flex: 0 0 auto; width: 32px; height: 32px; padding: 4px; display: flex; align-items: center; justify-content: center; }
  .ext-btn img { width: 100%; height: 100%; object-fit: contain; pointer-events: none; }
  /* A placeholder glyph for an extension with no resolvable icon
     (`ExtensionInfo.icon` was `none`) — still clickable, just with
     nothing to show but its initial. */
  .ext-btn .ext-fallback { font-size: 14px; }
  /* Reflects `popup-open` — a steady highlight, not just the
     transient `:hover` one, so the icon whose popup is currently open
     stays visually distinct while you're looking at something else. */
  .ext-btn.active { background: rgba(255, 255, 255, 0.16); }
  /* Sits at the bottom of the tab list, not with the tabs themselves
     (`#tabs` is rebuilt wholesale on every poll — see `refresh()` —
     so a button living inside it would get wiped and need
     re-creating every 500ms for no reason). */
  #new-tab-btn { margin-top: auto; }
</style>
</head>
<body>
<div id="sidebar">
  <div id="extensions"></div>
  <div id="tabs"></div>
  <button id="new-tab-btn" title="New Tab">+ New Tab</button>
</div>
<script>
  const windowId = window.__rashomonWindowId || '';

  function query(request, onSuccess) {
    window.cefQuery({
      request: windowId + ':' + request,
      onSuccess: onSuccess || function () {},
      onFailure: function () {},
    });
  }

  // Lets any non-interactive area of the sidebar (not a button, link,
  // etc.) drag-move the window, the same way clicking a real titlebar
  // would — this window's own titlebar is hidden (see
  // `rashomon-kernel`'s `WinitKernelApp::resumed`), and
  // `with_movable_by_window_background` alone doesn't reach here since
  // this whole region is a `wry` webview covering the window's
  // background, not the background itself. `window.ipc.postMessage`
  // directly, not the `cefQuery`/`query()` shim above — this is a
  // one-way host-chrome signal, not a Facet request with a response
  // to wait for.
  document.getElementById('sidebar').addEventListener('mousedown', function (e) {
    if (e.target.closest('button, a, input, textarea, select')) return;
    window.ipc.postMessage(JSON.stringify({ type: 'start-drag' }));
  });

  // Same direct `window.ipc.postMessage` bypass as the drag handler
  // above, not the `cefQuery`/`query()` shim — opening the new-tab
  // chooser is a host-chrome action (see `rashomon-kernel`'s
  // `mac::FacetWebView`'s `"new-tab"` IPC branch), not a Facet
  // request this Component's own `handle-input` ever sees.
  document.getElementById('new-tab-btn').addEventListener('click', function () {
    window.ipc.postMessage(JSON.stringify({ type: 'new-tab' }));
  });

  let lastPollKey = '';
  function refresh() {
    query('__poll__', function (response) {
      if (response === lastPollKey) return;
      lastPollKey = response;

      const extensionsContainer = document.getElementById('extensions');
      const tabsContainer = document.getElementById('tabs');
      extensionsContainer.innerHTML = '';
      tabsContainer.innerHTML = '';

      (response || '').split('\n').filter(Boolean).forEach(function (line) {
        const parts = line.split('\t');
        if (parts[0] === 'EXT') {
          const [, id, name, icon, popupOpen] = parts;
          const btn = document.createElement('button');
          btn.className = 'ext-btn' + (popupOpen === 'true' ? ' active' : '');
          btn.title = name;
          if (icon) {
            const img = document.createElement('img');
            img.src = icon;
            img.alt = name;
            btn.appendChild(img);
          } else {
            const fallback = document.createElement('span');
            fallback.className = 'ext-fallback';
            fallback.textContent = (name || '?').charAt(0).toUpperCase();
            btn.appendChild(fallback);
          }
          btn.onclick = function () {
            query('toggle-popup:' + id);
          };
          extensionsContainer.appendChild(btn);
        } else if (parts[0] === 'TAB') {
          const [, id, label] = parts;
          const btn = document.createElement('button');
          btn.textContent = label || id;
          btn.title = label || id;
          btn.onclick = function () {
            query('switch-tab:' + id);
          };
          tabsContainer.appendChild(btn);
        }
      });
    });
  }

  setInterval(refresh, 500);
  refresh();
</script>
</body>
</html>"#;

bindings::export!(Component with_types_in bindings);
