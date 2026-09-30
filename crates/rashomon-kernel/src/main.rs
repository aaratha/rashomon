//! Thin entry point for the CEF *browser* process — see `lib.rs` for
//! why this crate is a library with two binaries (this one and
//! `helper.rs`) on top of it, and for everything this delegates to.

use anyhow::{anyhow, ensure, Result};
use cef::*;
use rashomon_kernel::{make_minimal_app, run_browser_process};

fn main() -> Result<()> {
    #[cfg(target_os = "macos")]
    let _library = {
        let loader = library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), false);
        ensure!(loader.load(), "failed to load the CEF framework");
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        rashomon_kernel::mac::setup_kernel_application();
        loader
    };
    #[cfg(not(target_os = "macos"))]
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = args::Args::new();
    let cmd_line = args
        .as_cmd_line()
        .ok_or_else(|| anyhow!("failed to parse CEF command line arguments"))?;
    let is_browser_process = cmd_line.has_switch(Some(&CefString::from("type"))) != 1;

    // Passed to `execute_process` regardless of process role — but on
    // macOS this binary is only ever actually invoked as the browser
    // process; `bundle-cef-app` routes subprocess launches to the
    // separate `rashomon-kernel-helper` binary instead (see helper.rs).
    // Kept here anyway for cross-platform correctness (Linux/Windows
    // can re-exec the same binary for subprocess roles).
    let mut early_app = make_minimal_app();
    let ret = execute_process(Some(args.as_main_args()), Some(&mut early_app), std::ptr::null_mut());
    if !is_browser_process {
        ensure!(ret >= 0, "CEF subprocess execute_process failed");
        return Ok(());
    }
    ensure!(ret == -1, "browser process execute_process should return -1");

    run_browser_process(&args)
}
