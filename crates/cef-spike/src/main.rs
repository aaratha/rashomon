//! Standalone CEF bootstrap validation — does `cef-rs` even link and show
//! a window on this machine, without sandboxing or app-bundle helper
//! processes? Not wired into any Rashomon primitive/Component yet; see
//! `rashomon-kernel`'s module doc comment for where this is headed.

#[cfg(target_os = "macos")]
mod mac;

use cef::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

wrap_client! {
    pub struct SpikeClient {
        inner: Arc<Mutex<SpikeState>>,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(SpikeLifeSpanHandler::new(self.inner.clone()))
        }
    }
}

struct SpikeState {
    browser_count: u32,
}

wrap_life_span_handler! {
    struct SpikeLifeSpanHandler {
        inner: Arc<Mutex<SpikeState>>,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, _browser: Option<&mut Browser>) {
            self.inner.lock().expect("lock poisoned").browser_count += 1;
            println!("cef-spike: browser created");
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            let mut state = self.inner.lock().expect("lock poisoned");
            state.browser_count -= 1;
            println!("cef-spike: browser closed, {} remaining", state.browser_count);
            if state.browser_count == 0 {
                quit_message_loop();
            }
        }
    }
}

wrap_browser_process_handler! {
    struct SpikeBrowserProcessHandler {
        client: RefCell<Option<Client>>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            println!("cef-spike: context initialized, creating browser");

            let mut client = SpikeClient::new(Arc::new(Mutex::new(SpikeState { browser_count: 0 })));
            *self.client.borrow_mut() = Some(client.clone());

            let window_info = WindowInfo {
                runtime_style: RuntimeStyle::ALLOY,
                ..Default::default()
            };
            let settings = BrowserSettings::default();

            let html = "data:text/html,<html><body style=\"background:%23202830;color:%23e0e0e0;font-family:monospace;font-size:2em\"><p>rashomon: cef-spike is alive</p></body></html>";
            let url = CefString::from(html);

            browser_host_create_browser(
                Some(&window_info),
                Some(&mut client),
                Some(&url),
                Some(&settings),
                None,
                None,
            );
        }
    }
}

wrap_app! {
    pub struct SpikeApp;

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(SpikeBrowserProcessHandler::new(RefCell::new(None)))
        }
    }
}

fn main() -> Result<(), &'static str> {
    #[cfg(target_os = "macos")]
    let _library = {
        let loader = library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), false);
        assert!(loader.load());
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        mac::setup_spike_application();
        loader
    };
    #[cfg(not(target_os = "macos"))]
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = args::Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        return Err("failed to parse command line arguments");
    };

    let switch = CefString::from("type");
    let is_browser_process = cmd_line.has_switch(Some(&switch)) != 1;

    let ret = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());

    if !is_browser_process {
        // A CEF subprocess (renderer/GPU/etc.) re-executing this same
        // binary — execute_process has already dispatched it. Don't
        // initialize CEF again in this process.
        assert!(ret >= 0, "subprocess execute_process failed");
        return Ok(());
    }
    assert_eq!(ret, -1, "browser process execute_process should return -1");

    let mut app = SpikeApp::new();
    let settings = Settings {
        no_sandbox: 1,
        ..Default::default()
    };
    assert_eq!(
        initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()),
        1,
        "cef initialize() failed"
    );

    #[cfg(target_os = "macos")]
    let _delegate = mac::setup_spike_app_delegate();

    run_message_loop();
    shutdown();

    Ok(())
}
