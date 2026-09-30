//! CEF's macOS subprocess entry point — `bundle-cef-app` routes
//! renderer/GPU/utility subprocess launches to *this* binary (a
//! separate bundled `.app`), not back through `main.rs`. It needs a
//! real `KernelApp` (not `None`), because the renderer subprocess
//! specifically must get `render_process_handler()` called on it to
//! register `window.cefQuery` in the page's JS context — see the
//! `lib.rs` module doc comment for why this crate is a library with two
//! binaries on top of it. Everything else is unchanged from the
//! `cef-spike` validation.

use cef::*;
use rashomon_kernel::make_minimal_app;

fn main() {
    let args = args::Args::new();

    let _loader = {
        let loader = library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), true);
        assert!(loader.load());
        loader
    };

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let mut app = make_minimal_app();
    execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut());
}
