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
    /// and does the parsing/rendering itself, client-side, once
    /// there's a CEF-backed `rashomon:ui` that can actually run it. For
    /// now `render` just embeds a snapshot of whatever the PTY has
    /// produced so far as the page's initial `term.write(...)` call —
    /// there's no live host<->page channel yet, so it isn't
    /// incremental the way the real thing will be.
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

  // TODO(rashomon:ui): once a CEF-backed message bridge exists, wire
  // this to call `handle-input` on the Facet instead of just logging —
  // and likewise, the host should push new PTY bytes into `term.write`
  // as they arrive instead of `render` only ever describing a static
  // snapshot the way it does today.
  term.onData((data) => {{
    console.log('terminal-xterm: onData (not yet wired to a host bridge):', data);
  }});
</script>
</body>
</html>"#,
        initial = js_string_escape(initial_output),
    )
}

bindings::export!(Component with_types_in bindings);
