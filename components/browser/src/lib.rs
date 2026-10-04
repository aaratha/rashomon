#[allow(warnings)]
mod bindings;

use std::cell::RefCell;

use bindings::exports::rashomon::facet::contract::Guest;
use bindings::rashomon::browser::control;
use bindings::rashomon::browser::types::{BrowserContext, BrowserTab};

/// The two starter tabs this demo opens — the exact pair
/// `cef-extension-spike` used to validate tab switching.
const STARTER_URLS: [&str; 2] = ["https://example.com/", "https://www.wikipedia.org/"];

/// Persists across calls within this Component's one instantiation —
/// see `terminal`'s identical `SESSION` pattern. Without this, the
/// `BrowserTab`/`BrowserContext` resource handles would be dropped
/// (closing the underlying browsers) the instant `render()` returns.
struct Session {
    #[allow(dead_code)]
    context: BrowserContext,
    tabs: Vec<BrowserTab>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

struct Component;

impl Guest for Component {
    /// On first call, opens one `browser-tab` per starter URL — the
    /// host mounts/switches between them in its own native switcher
    /// window (see `rashomon-kernel`'s `BrowserSwitcherState`). Graph
    /// tracking for each tab's URL (finding or creating its
    /// `rashomon:page` Entity, then recording an Occurrence for
    /// further navigations) is entirely the host's own job now —
    /// `rashomon-kernel`'s `TabDisplayHandler::on_address_change`
    /// handles it for every real browser tab uniformly, these two
    /// starter tabs included, not just ones a Facet happens to record
    /// by hand. There's no interactive HTML surface here; the real UI
    /// is the native switcher window the host builds, not a Facet page.
    fn render(_node_id: String) -> String {
        SESSION.with_borrow_mut(|session| {
            if session.is_none() {
                let context = control::create_context();
                let tabs = STARTER_URLS.iter().map(|url| control::create_tab(&context, url)).collect();
                *session = Some(Session { context, tabs });
            }

            let session = session.as_ref().expect("just ensured Some above");
            let summary = session
                .tabs
                .iter()
                .map(|tab| format!("{} ({})", tab.id(), tab.current_url()))
                .collect::<Vec<_>>()
                .join(", ");
            format!("browser: opened tabs: {summary}")
        })
    }

    fn handle_input(event: String) -> Vec<String> {
        vec![format!("browser saw input: {event}")]
    }

    fn poll_output() -> String {
        String::new()
    }
}

bindings::export!(Component with_types_in bindings);
