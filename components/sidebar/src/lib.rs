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
    /// stripped by the host, same as every other Facet — `toggle-popup`
    /// and `switch-tab:<id>` are this Component's own tiny protocol,
    /// not something the host knows the shape of.
    fn handle_input(event: String) -> Vec<String> {
        CONTEXT.with_borrow(|ctx| {
            let Some(ctx) = ctx.as_ref() else { return };
            if event == "toggle-popup" {
                // Toggles the first configured extension's popup —
                // there's only ever one configured right now (Bitwarden,
                // in `rashomon-kernel::run_browser_process`), so "first"
                // is unambiguous for now.
                if let Some(ext) = ctx.list_extensions().into_iter().next() {
                    let _ = ctx.toggle_extension_popup(&ext.id);
                }
            } else if let Some(tab_id) = event.strip_prefix("switch-tab:") {
                let _ = ctx.switch_to_tab(tab_id);
            }
        });
        Vec::new()
    }

    /// Polled on a `setInterval`, same as `terminal-xterm`'s live
    /// output — returns the current tab list as `id\tlabel` lines, no
    /// incremental diffing needed since `list-tabs` is cheap and the
    /// page itself only re-renders when the result actually changes.
    fn poll_output() -> String {
        CONTEXT.with_borrow(|ctx| {
            let Some(ctx) = ctx.as_ref() else { return String::new() };
            ctx.list_tabs()
                .into_iter()
                .map(|tab| format!("{}\t{}", tab.id, tab.title))
                .collect::<Vec<_>>()
                .join("\n")
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
    /* Translucent rather than solid — matches the sidebar's own glass
       background instead of sitting on top of it as an opaque block. */
    background: rgba(255, 255, 255, 0.08);
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
</style>
</head>
<body>
<div id="sidebar">
  <button id="popup-btn">Open Popup</button>
  <div id="tabs"></div>
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

  document.getElementById('popup-btn').onclick = function () {
    query('toggle-popup');
  };

  let lastTabsKey = '';
  function refreshTabs() {
    query('__poll__', function (response) {
      if (response === lastTabsKey) return;
      lastTabsKey = response;
      const container = document.getElementById('tabs');
      container.innerHTML = '';
      (response || '').split('\n').filter(Boolean).forEach(function (line) {
        const parts = line.split('\t');
        const id = parts[0];
        const label = parts[1] || id;
        const btn = document.createElement('button');
        btn.textContent = label;
        btn.title = label;
        btn.onclick = function () {
          query('switch-tab:' + id);
        };
        container.appendChild(btn);
      });
    });
  }

  setInterval(refreshTabs, 500);
  refreshTabs();
</script>
</body>
</html>"#;

bindings::export!(Component with_types_in bindings);
