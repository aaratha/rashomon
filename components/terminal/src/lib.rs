#[allow(warnings)]
mod bindings;

use std::cell::RefCell;

use bindings::exports::rashomon::facet::contract::Guest;
use bindings::rashomon::graph::store;
use bindings::rashomon::graph::types::{Property, Role};
use bindings::rashomon::process::spawner::{self, Process};

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// Persists across `render`/`handle-input` calls within this
/// Component's one instantiation — a wasm32-wasip1 guest is
/// single-threaded under our synchronous host, so `thread_local!` (no
/// real concurrency, no `Send`/`Sync` needed) is enough to keep the
/// spawned shell and its parsed screen state alive between calls,
/// the same way a native process would just use a global.
struct Session {
    process: Process,
    parser: vt100::Parser,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

struct Component;

impl Guest for Component {
    /// On first call, spawns an interactive shell over the real PTY
    /// `rashomon:process` now provides, and records the session as a
    /// `shell-session` Occurrence attached to `node_id` via
    /// `occurrence-of` (the doc's own worked example). Every call —
    /// first or not — drains whatever new output has arrived since the
    /// last one into the `vt100` parser and returns the current screen
    /// contents, so a poll-driven redraw loop sees live output even
    /// between keystrokes.
    fn render(node_id: String) -> String {
        SESSION.with_borrow_mut(|session| {
            if session.is_none() {
                let process = match spawner::spawn("bash", &[], None) {
                    Ok(process) => process,
                    Err(err) => return format!("terminal: failed to spawn: {err}"),
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

                *session = Some(Session {
                    process,
                    parser: vt100::Parser::new(ROWS, COLS, 1000),
                });
            }

            let session = session.as_mut().expect("just ensured Some above");
            loop {
                match session.process.read(4096) {
                    Ok(chunk) if chunk.is_empty() => break,
                    Ok(chunk) => session.parser.process(&chunk),
                    Err(err) => return format!("terminal: failed to read output: {err}"),
                }
            }
            session.parser.screen().contents()
        })
    }

    /// `event` is already-encoded terminal input bytes (as a UTF-8
    /// string) — the host is responsible for turning a raw key event
    /// into the correct byte sequence before calling this, the same
    /// way a real terminal emulator's key encoder would.
    fn handle_input(event: String) -> Vec<String> {
        SESSION.with_borrow_mut(|session| {
            if let Some(session) = session.as_mut() {
                let _ = session.process.write(event.as_bytes());
            }
        });
        Vec::new()
    }
}

bindings::export!(Component with_types_in bindings);
