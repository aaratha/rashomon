#[allow(warnings)]
mod bindings;

use bindings::exports::rashomon::facet::facet::Guest;

struct Component;

impl Guest for Component {
    fn render(_node_id: String) -> String {
        "ping: hello from the ping component".to_string()
    }

    fn handle_input(_event: String) -> Vec<String> {
        Vec::new()
    }
}

bindings::export!(Component with_types_in bindings);
