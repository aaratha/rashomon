#[allow(warnings)]
mod bindings;

use bindings::exports::rashomon::facet::contract::Guest;
use bindings::rashomon::graph::store;
use bindings::rashomon::graph::types::{Property, Role};
use bindings::rashomon::process::spawner;

struct Component;

impl Guest for Component {
    /// Spawns `echo`, captures its output, then records the run as a
    /// `shell-session` Occurrence attached to the Entity named by
    /// `node_id` via `occurrence-of` — the doc's own worked example
    /// (`shell-session (occurrence), one PTY-backed terminal run`).
    /// This is a one-shot, run-to-completion capture, not an
    /// interactive terminal — see `terminal` for that.
    fn render(node_id: String) -> String {
        let process = match spawner::spawn("echo", &["hello from rashomon".to_string()], None) {
            Ok(process) => process,
            Err(err) => return format!("shell: failed to spawn: {err}"),
        };

        // `read` is non-blocking (a live terminal's redraw loop must
        // never stall waiting on it) — so unlike a blocking pipe read,
        // an empty result here means "nothing *yet*", not "done". The
        // host's `wait()` joins its background reader thread before
        // returning, guaranteeing `read` calls after this point see the
        // process's entire output — so a single drain-until-empty pass
        // is safe here, unlike it would be before waiting.
        let exit_code = process.wait();
        let mut output = Vec::new();
        loop {
            match process.read(4096) {
                Ok(chunk) if chunk.is_empty() => break,
                Ok(mut chunk) => output.append(&mut chunk),
                Err(err) => return format!("shell: failed to read output: {err}"),
            }
        }
        let output_text = String::from_utf8_lossy(&output).trim().to_string();

        let occurrence = store::create_node(
            "rashomon:shell-session",
            Role::Occurrence,
            &[
                Property {
                    key: "cwd".to_string(),
                    value: ".".to_string(),
                },
                Property {
                    key: "exit-code".to_string(),
                    value: exit_code.to_string(),
                },
            ],
        );
        store::create_edge("occurrence-of", &occurrence.id, &node_id, 1.0);

        format!("shell: ran `echo` (exit {exit_code}), output: {output_text:?}, recorded as {}", occurrence.id)
    }

    fn handle_input(_event: String) -> Vec<String> {
        Vec::new()
    }
}

bindings::export!(Component with_types_in bindings);
