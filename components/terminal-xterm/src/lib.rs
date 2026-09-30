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
    /// that arrives via `poll_output` below, which the host calls on a
    /// recurring timer and injects into the already-loaded page. Input
    /// (`term.onData` -> `window.cefQuery` -> `handle_input` below) is
    /// real too, wired through rashomon-kernel's CEF message router.
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
    /// `vt100` Facet. Once `rashomon:ui` exists, this is where a
    /// message from the page's `term.onData` handler would arrive
    /// instead of a host-encoded key event.
    fn handle_input(event: String) -> Vec<String> {
        SESSION.with_borrow_mut(|session| {
            if let Some(process) = session.as_ref() {
                let _ = process.write(event.as_bytes());
            }
        });
        Vec::new()
    }

    /// Drains whatever PTY output has arrived since the last call (from
    /// `render` or this) and returns it as plain text — raw, not
    /// JS-escaped, since the host does its own (base64-based, so it's
    /// binary-safe) encoding when building the `term.write(...)` call
    /// this feeds. Empty string if the session doesn't exist yet or
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
  // context (see rashomon-kernel's RenderProcessHandler) and forwards
  // the request string verbatim to this Facet's handle-input.
  //
  // Live output between renders arrives via a separate
  // `term.write(...)` call the host injects on a recurring poll of
  // `poll-output` (see rashomon-kernel's `OutputPollTask`) — this
  // initial `term.write` above only covers whatever had already
  // buffered up before the page loaded.
  term.onData((data) => {{
    window.cefQuery({{
      request: data,
      onSuccess: function () {{}},
      onFailure: function () {{}},
    }});
  }});
</script>
</body>
</html>"#,
        initial = js_string_escape(initial_output),
    )
}

bindings::export!(Component with_types_in bindings);
