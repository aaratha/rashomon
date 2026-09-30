//! CEF's macOS subprocess entry point — the main binary re-executes
//! *this* tiny helper (via a separate bundled `.app`) to become a
//! renderer/GPU/etc. process. No sandbox feature, so no `Sandbox` setup
//! here, unlike the upstream `cefsimple` helper.

use cef::*;

fn main() {
    let args = args::Args::new();

    let _loader = {
        let loader = library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), true);
        assert!(loader.load());
        loader
    };

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    execute_process(Some(args.as_main_args()), None::<&mut App>, std::ptr::null_mut());
}
