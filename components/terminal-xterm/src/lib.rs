#[allow(warnings)]
mod bindings;

use std::cell::RefCell;

use bindings::exports::rashomon::facet::contract::Guest;
use bindings::rashomon::graph::store;
use bindings::rashomon::graph::types::{Property, Role};
use bindings::rashomon::process::spawner::{self, Process};

thread_local! {
    static SESSION: RefCell<Option<Process>> = const { RefCell::new(None) };
}

/// Vendored, not loaded from a CDN `<script src>` — this Facet is core
/// local tooling (a terminal emulator), not something that should
/// stop working without a live internet connection just because of
/// how its UI library happens to be fetched. Embedded into the
/// compiled Component itself via `include_str!`, so the served page
/// below inlines the actual source instead of a URL.
const XTERM_JS: &str = include_str!("../vendor/xterm.js");
const XTERM_CSS: &str = include_str!("../vendor/xterm.css");

struct Component;

impl Guest for Component {
    /// The `vt100`-based `terminal` Facet parses PTY bytes on the guest
    /// side and hands the host plain text. This Facet does none of
    /// that — it hands the host an HTML document that loads `xterm.js`
    /// and does the parsing/rendering itself, client-side. `render`
    /// only embeds a snapshot of whatever the PTY has produced so far,
    /// as the page's initial `term.write(...)` call — anything after
    /// that is pulled via `poll_output` below, which the page itself
    /// polls on a `setInterval` through the same `cefQuery` channel
    /// input uses. Input (`term.onData` -> `window.cefQuery` ->
    /// `handle_input` below) is real too, wired through
    /// rashomon-kernel's CEF message router. This Facet is instantiated
    /// once per open View, each with its own instance of the user's
    /// default shell (`spawner::default-shell` — `SESSION` is
    /// per-instance) — but a View isn't the same as a Window:
    /// `Kernel::open_window` can mirror one View's `render` output into
    /// more than one Window, all sharing this exact instance (and so
    /// this exact shell session), which is why this guest has no
    /// notion of "Window" at all — that routing is purely host-side.
    fn render(node_id: String) -> String {
        SESSION.with_borrow_mut(|session| {
            if session.is_none() {
                let shell = spawner::default_shell();
                let process = match spawner::spawn(&shell, &[], None) {
                    Ok(process) => process,
                    Err(err) => return format!("terminal-xterm: failed to spawn: {err}"),
                };

                let occurrence = store::create_node(
                    "rashomon:shell-session",
                    Role::Occurrence,
                    &[Property {
                        key: "cwd".to_string(),
                        value: ".".to_string(),
                    }],
                );
                store::create_edge("occurrence-of", &occurrence.id, &node_id, 1.0);

                *session = Some(process);
            }

            let process = session.as_ref().expect("just ensured Some above");
            let mut output = Vec::new();
            loop {
                match process.read(4096) {
                    Ok(chunk) if chunk.is_empty() => break,
                    Ok(chunk) => output.extend(chunk),
                    Err(err) => return format!("terminal-xterm: failed to read output: {err}"),
                }
            }

            render_page(&String::from_utf8_lossy(&output))
        })
    }

    /// Forwards already-encoded terminal input to the PTY, same as the
    /// `vt100` Facet. `event` has already had its `<window-id>:` routing
    /// prefix stripped by the host's `InputQueryHandler` before this is
    /// called — this Facet has no notion of Windows, only PTY bytes.
    ///
    /// One exception: a leading `"\u{1}"` marks a resize notification
    /// (`"\u{1}resize:<cols>,<rows>"`) rather than real keystroke/paste
    /// bytes — the page's own JS (see `render_page`) sends this
    /// whenever it refits `xterm.js`'s grid to its actual on-screen
    /// size, so the real PTY (and whatever's running in it, e.g. a
    /// full-screen program) agrees on the terminal's dimensions
    /// instead of staying stuck at the hardcoded 80x24 this Facet used
    /// to always open with. `\u{1}` (SOH) rather than a plain word
    /// prefix like `"resize:"` specifically because this channel *is*
    /// otherwise raw PTY bytes verbatim — picked as a control
    /// character no real keystroke or paste ever actually produces,
    /// so it can't collide with legitimate input the way a printable
    /// prefix could.
    fn handle_input(event: String) -> Vec<String> {
        if let Some(dims) = event.strip_prefix('\u{1}').and_then(|s| s.strip_prefix("resize:")) {
            if let Some((cols, rows)) = dims.split_once(',') {
                if let (Ok(cols), Ok(rows)) = (cols.parse::<u32>(), rows.parse::<u32>()) {
                    SESSION.with_borrow(|session| {
                        if let Some(process) = session.as_ref() {
                            let _ = process.resize(cols, rows);
                        }
                    });
                }
            }
            return Vec::new();
        }
        SESSION.with_borrow_mut(|session| {
            if let Some(process) = session.as_ref() {
                let _ = process.write(event.as_bytes());
            }
        });
        Vec::new()
    }

    /// Drains whatever PTY output has arrived since the last call (from
    /// `render` or this) and returns it as plain text. No JS-escaping
    /// needed here — the host hands this string back to the page's
    /// `cefQuery` `onSuccess` callback as a native value, not source
    /// text to `eval`, so it arrives as a real, already-Unicode-correct
    /// JS string. Empty string if the session doesn't exist yet or
    /// nothing new has arrived.
    fn poll_output() -> String {
        SESSION.with_borrow(|session| {
            let Some(process) = session.as_ref() else {
                return String::new();
            };
            let mut output = Vec::new();
            loop {
                match process.read(4096) {
                    Ok(chunk) if chunk.is_empty() => break,
                    Ok(chunk) => output.extend(chunk),
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&output).into_owned()
        })
    }
}

/// Escapes `s` for embedding inside a single-quoted JS string literal —
/// just the handful of characters that matter (backslash, quote,
/// newline, carriage return); not a general JSON encoder, since this is
/// the only place that needs it.
fn js_string_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

fn render_page(initial_output: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
<title>Terminal</title>
<meta charset="utf-8" />
<style>{xterm_css}</style>
<style>
  /* `height: 100%` on both — the default `html`/`body` only grow to
     fit their content's natural height, not the viewport, so without
     this the terminal grid below can only ever be as tall as whatever
     `xterm.js` happens to size itself to, not the other way around. */
  html, body {{ margin: 0; height: 100%; background: transparent; }}
  /* Fills the whole webview (itself already sized to the full content
     region by the host — see `mac::FacetWebView`) so `fitTerminal`
     below has the actual available space to measure against, not
     xterm.js's initial 80x24 default. */
  #terminal {{ width: 100%; height: 100%; }}
</style>
</head>
<body>
<div id="terminal"></div>
<script>{xterm_js}</script>
<script>
  // `allowTransparency` is required for xterm.js's own canvas renderer
  // to honor a non-opaque `theme.background` at all — without it, the
  // canvas paints fully opaque regardless of what's set here, which is
  // why just making the surrounding page transparent (above) wasn't
  // enough on its own.
  const term = new Terminal({{
    cols: 80,
    rows: 24,
    allowTransparency: true,
    theme: {{ background: 'rgba(0, 0, 0, 0)' }},
    // The "Mono" variant (not plain "...Nerd Font") is the one Nerd
    // Fonts patches to keep its icon/powerline glyphs exactly
    // one-cell-wide — without that, icons can throw off column
    // alignment in a real monospace grid like xterm.js's. Both names
    // are tried (a Nerd Fonts install can register either, depending
    // on how it was installed) before falling back to a generic
    // monospace font if neither is actually installed.
    fontFamily: "'JetBrainsMono Nerd Font Mono', 'JetBrainsMono Nerd Font', monospace",
  }});
  term.open(document.getElementById('terminal'));
  term.write('{initial}');

  // No bundled fit addon (this project vendors `xterm.js` itself
  // rather than pulling from a CDN — see the doc comment on
  // `XTERM_JS` — and the official `addon-fit` is a separate package
  // this doesn't vendor) — `_renderService.dimensions.css.cell` is
  // `_core`'s own already-measured real cell size in CSS pixels
  // (exactly what `addon-fit` itself reads internally; there's no
  // *public* API for this in xterm.js as of this vendored version),
  // reused here instead of re-deriving font metrics by hand, which
  // would drift from whatever xterm.js's own renderer actually used.
  function fitTerminal() {{
    const el = document.getElementById('terminal');
    const dims = term._core && term._core._renderService && term._core._renderService.dimensions;
    const cell = dims && dims.css && dims.css.cell;
    if (!cell || !cell.width || !cell.height) return;
    const cols = Math.max(2, Math.floor(el.clientWidth / cell.width));
    const rows = Math.max(1, Math.floor(el.clientHeight / cell.height));
    if (cols === term.cols && rows === term.rows) return;
    term.resize(cols, rows);
    // `windowId` isn't declared until below, but is already set by
    // the time this ever actually runs (only from the event listeners
    // further down, never synchronously here).
    window.cefQuery({{
      request: windowId + ':\u0001resize:' + cols + ',' + rows,
      onSuccess: function () {{}},
      onFailure: function () {{}},
    }});
  }}

  // The host's message router registers `window.cefQuery` in this
  // context (see rashomon-kernel's RenderProcessHandler) and routes it
  // by the `<window-id>:` prefix every request below carries —
  // `__rashomonWindowId` is whatever id the host assigned this Window
  // when it opened it, injected as a `<script>` right after `<head>`
  // before this document became this Window's page (see
  // `inject_window_id` in rashomon-kernel), so it's already set by the
  // time this runs. Several Windows can carry different ids for the
  // *same* underlying View — that's how one `bash` session ends up
  // mirrored across more than one Window.
  const windowId = window.__rashomonWindowId || '';

  term.onData((data) => {{
    window.cefQuery({{
      request: windowId + ':' + data,
      onSuccess: function () {{}},
      onFailure: function () {{}},
    }});
  }});

  // `window`'s size here tracks this webview's own frame, not some
  // unrelated top-level browser window — WebKit fires `resize` on it
  // whenever the host repositions/resizes the native view this page
  // is rendering into (see `mac::set_view_frame`), e.g. when this tab
  // becomes active again after another tab was. The immediate call
  // covers the very first layout, before any such event has fired.
  fitTerminal();
  window.addEventListener('resize', fitTerminal);

  // Live output between renders is *pulled*, not pushed: this page
  // polls itself rather than the host injecting `term.write(...)` via
  // `execute_java_script` from a timer on the host side, because that
  // turns out not to work — `execute_java_script` calls made from a
  // CEF `Task` (the natural way to drive a host-side push loop) are
  // silently no-ops, confirmed empirically. Routing the poll through
  // the same `cefQuery` channel real input already uses sidesteps the
  // problem entirely, since that round trip is a genuine Handler
  // callback the whole way, not a Task.
  setInterval(function () {{
    window.cefQuery({{
      request: windowId + ':__poll__',
      onSuccess: function (output) {{
        if (output) term.write(output);
      }},
      onFailure: function () {{}},
    }});
  }}, 50);
</script>
</body>
</html>"#,
        initial = js_string_escape(initial_output),
        xterm_css = XTERM_CSS,
        xterm_js = XTERM_JS,
    )
}

bindings::export!(Component with_types_in bindings);
