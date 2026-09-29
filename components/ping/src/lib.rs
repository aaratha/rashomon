#[allow(warnings)]
mod bindings;

use bindings::exports::rashomon::facet::contract::Guest;

struct Component;

impl Guest for Component {
    fn render(_node_id: String) -> String {
        "ping: hello from the ping component".to_string()
    }

    fn handle_input(event: String) -> Vec<String> {
        vec![format!("ping saw input: {event}")]
    }
}

bindings::export!(Component with_types_in bindings);
