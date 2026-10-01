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
    /// once per open View, each with its own `bash` (`SESSION` is
    /// per-instance) — but a View isn't the same as a Window:
    /// `Kernel::open_window` can mirror one View's `render` output into
    /// more than one Window, all sharing this exact instance (and so
    /// this exact `bash`), which is why this guest has no notion of
    /// "Window" at all — that routing is purely host-side.
    fn render(node_id: String) -> String {
        SESSION.with_borrow_mut(|session| {
            if session.is_none() {
                let process = match spawner::spawn("bash", &[], None) {
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
    fn handle_input(event: String) -> Vec<String> {
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
<meta charset="utf-8" />
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@xterm/xterm@5/css/xterm.css" />
<style>html, body {{ margin: 0; background: #202830; }}</style>
</head>
<body>
<div id="terminal"></div>
<script src="https://cdn.jsdelivr.net/npm/@xterm/xterm@5/lib/xterm.js"></script>
<script>
  const term = new Terminal({{ cols: 80, rows: 24 }});
  term.open(document.getElementById('terminal'));
  term.write('{initial}');

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
    )
}

bindings::export!(Component with_types_in bindings);
