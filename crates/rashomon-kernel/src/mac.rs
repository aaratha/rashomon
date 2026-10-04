//! macOS needs `NSApplication`/`NSApplicationDelegate` to conform to
//! CEF's Objective-C protocols for event handling to work at all — this
//! is the same setup validated in `crates/cef-spike`, just renamed.
//!
//! Also hosts this platform's half of the winit+CEF-classic+wry
//! architecture validated in `crates/cef-winit-spike`: a single native
//! window (owned by `winit`, see `run_browser_process`), with CEF
//! browser tabs embedded as classic (non-Views) child browsers and
//! Facet View tabs/the sidebar/the urlbar hosted as transparent `wry`
//! webviews over a real, private-API background blur
//! (`CGSSetWindowBackgroundBlurRadius`) — see [`apply_background_blur`].

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApp, NSApplication, NSApplicationDelegate, NSColor, NSEvent, NSView};
use objc2_core_foundation::CGRect;
use raw_window_handle::{AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, WindowHandle};
use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

define_class! {
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct KernelAppDelegate;

    impl KernelAppDelegate {
        #[unsafe(method(createApplication:))]
        unsafe fn create_application(&self, _object: Option<&AnyObject>) {
            let app = NSApp(MainThreadMarker::new().expect("not on main thread"));
            assert!(app.isKindOfClass(KernelApplication::class()));
        }
    }

    unsafe impl NSObjectProtocol for KernelAppDelegate {}

    unsafe impl NSApplicationDelegate for KernelAppDelegate {}
}

impl KernelAppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = KernelAppDelegate::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Default)]
pub struct KernelApplicationIvars {
    handling_send_event: Cell<Bool>,
}

define_class!(
    #[unsafe(super(NSApplication))]
    #[ivars = KernelApplicationIvars]
    pub struct KernelApplication;

    impl KernelApplication {
        #[unsafe(method(sendEvent:))]
        unsafe fn send_event(&self, event: &NSEvent) {
            let was_sending = self.is_handling_send_event();
            if !was_sending {
                self.set_handling_send_event(true);
            }
            let _: () = msg_send![super(self), sendEvent: event];
            if !was_sending {
                self.set_handling_send_event(false);
            }
        }
    }

    unsafe impl CrAppControlProtocol for KernelApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        unsafe fn _set_handling_send_event(&self, handling_send_event: Bool) {
            self.ivars().handling_send_event.set(handling_send_event);
        }
    }

    unsafe impl CrAppProtocol for KernelApplication {
        #[unsafe(method(isHandlingSendEvent))]
        unsafe fn _is_handling_send_event(&self) -> Bool {
            self.ivars().handling_send_event.get()
        }
    }

    unsafe impl CefAppProtocol for KernelApplication {}
);

impl KernelApplication {
    fn set_handling_send_event(&self, handling_send_event: bool) {
        unsafe { msg_send![self, setHandlingSendEvent: handling_send_event] }
    }

    fn is_handling_send_event(&self) -> bool {
        unsafe { msg_send![self, isHandlingSendEvent] }
    }
}

pub fn setup_kernel_application() {
    let mtm = MainThreadMarker::new().expect("not on main thread");
    let app: Retained<NSApplication> = unsafe { msg_send![KernelApplication::class(), sharedApplication] };
    let _ = mtm;
    assert!(app.isKindOfClass(KernelApplication::class()));
}

pub fn setup_kernel_app_delegate() -> Retained<KernelAppDelegate> {
    let mtm = MainThreadMarker::new().expect("not on main thread");
    let delegate = KernelAppDelegate::new(mtm);
    let app = NSApp(mtm);
    let proto = objc2::runtime::ProtocolObject::from_ref(&*delegate);
    app.setDelegate(Some(proto));

    unsafe {
        let _: () = msg_send![&delegate, performSelectorOnMainThread: sel!(createApplication:), withObject: std::ptr::null::<AnyObject>(), waitUntilDone: false];
    }

    delegate
}

/// Wraps the raw `NSView*` for `winit`'s window's content view (from
/// `raw_window_handle::AppKitWindowHandle::ns_view`) so both `wry`
/// (which expects a `raw_window_handle::HasWindowHandle`, an ecosystem
/// CEF's own Rust bindings predate and don't participate in) and CEF's
/// classic child-browser embedding (`WindowInfo::set_as_child`, which
/// just wants the same raw pointer cast to `cef_window_handle_t`) can
/// attach to the one native window `winit` created — see
/// `run_browser_process`'s `ApplicationHandler::resumed`.
#[derive(Clone, Copy)]
pub struct NativeWindowHandle(NonNull<std::ffi::c_void>);

impl NativeWindowHandle {
    pub(crate) fn from_ns_view_ptr(ptr: *mut std::ffi::c_void) -> Option<Self> {
        NonNull::new(ptr).map(Self)
    }
}

impl HasWindowHandle for NativeWindowHandle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::AppKit(AppKitWindowHandle::new(self.0));
        // Safe: `self.0` stays valid for as long as `winit`'s `Window`
        // (and thus its backing `NSView`) is alive, which outlives
        // every `wry::WebView`/CEF child browser this handle is used
        // to create — they're torn down well before the window itself
        // ever closes.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

// `NonNull` has no automatic `Send`/`Sync` — asserted manually here
// under the same invariant every other native-window type in this
// codebase relies on: everything touching it stays on CEF/winit's one
// UI thread, synchronized through `Arc<Mutex<_>>` at the
// `BrowserSwitcherState` level, never actually accessed concurrently.
unsafe impl Send for NativeWindowHandle {}
unsafe impl Sync for NativeWindowHandle {}

/// Repositions/resizes a native child view in place — used to move
/// whichever tab widget (a CEF classic child browser's own `NSView*`,
/// or a `wry` webview's) is becoming active into the content region,
/// since both CEF and `wry` otherwise leave a child view wherever it
/// was first created. `rect` is in `winit`'s window-relative, top-left-
/// origin, y-down coordinate space — the same one CEF's `Rect` uses,
/// so callers can pass that straight through.
pub(crate) fn set_view_frame(ns_view_ptr: *mut std::ffi::c_void, rect: cef::Rect) {
    let ns_view = unsafe { &*(ns_view_ptr as *const NSView) };
    ns_view.setFrame(CGRect {
        origin: objc2_core_foundation::CGPoint { x: rect.x as f64, y: rect.y as f64 },
        size: objc2_core_foundation::CGSize { width: rect.width as f64, height: rect.height as f64 },
    });
}

/// Shows or hides a native child view without destroying it — used to
/// switch tabs by hiding whichever one was active and showing the new
/// one, rather than tearing either down.
pub(crate) fn set_view_hidden(ns_view_ptr: *mut std::ffi::c_void, hidden: bool) {
    let ns_view = unsafe { &*(ns_view_ptr as *const NSView) };
    ns_view.setHidden(hidden);
}

// Private, undocumented CoreGraphics Services (CGS) API — not in any
// public SDK header, so it's declared here by hand. This is the exact
// call iTerm2, Terminal.app, and Ghostty use for real background
// blur, and `winit` itself ships it (behind an explicit opt-in Cargo
// feature, acknowledging the same "private API" tradeoff). It's been
// stable across macOS releases for well over a decade; the risk is
// Apple silently changing or removing it in a future release (no
// App Store distribution path either), not that it's flaky today.
// Public `NSVisualEffectView`/`NSGlassEffectView` materials were
// tried first and capped out at a barely-visible effect regardless of
// material/style — this is the actual fix.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGSDefaultConnectionForThread() -> i32;
    fn CGSSetWindowBackgroundBlurRadius(connection: i32, window_number: i32, radius: i32) -> i32;
}

/// `ns_view_ptr` is the raw `NSView*` for the window's content view —
/// recovers its owning `NSWindow` and applies blur to the window as a
/// whole (this API is per-window, not per-region; CEF's own opaque
/// pixels, wherever a real browser tab is embedded, naturally hide the
/// blur underneath them — that's what makes "transparent Facets show
/// blur, opaque browser pages don't" work for free with one call).
pub(crate) fn apply_background_blur(ns_view_ptr: *mut std::ffi::c_void, radius: i32) {
    let ns_view = unsafe { &*(ns_view_ptr as *const NSView) };
    let Some(window) = ns_view.window() else {
        eprintln!("apply_background_blur: view has no owning window yet");
        return;
    };
    // `winit`'s `with_transparent(true)` sets the window's background
    // to `NSColor.clearColor()` — alpha *exactly* 0. A well-documented
    // macOS quirk (already hit once before in this codebase's own
    // Liquid Glass work) is that the window server treats an
    // exactly-zero-alpha window as having no compositable backing at
    // all, which breaks blur sampling outright: the blur radius is
    // still set, there's just nothing for it to render into except
    // during a transient compositing pass (e.g. the Stage Manager
    // focus-in animation, which uses its own temporary backing) —
    // explaining why it only ever flashed into view there and was
    // fully transparent otherwise. Overwriting it with a nonzero
    // alpha gives the window server real backing to blur, and
    // doubles as a dark overlay tint that helps the blurred content
    // underneath stay legible, the same way every real translucent
    // macOS panel (Terminal, menu bar, Control Center) tints rather
    // than being pure glass.
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.0, 0.6)));
    unsafe {
        let connection = CGSDefaultConnectionForThread();
        CGSSetWindowBackgroundBlurRadius(connection, window.windowNumber() as i32, radius);
    }
}

/// A tiny polyfill making `window.cefQuery({request, onSuccess, onFailure})`
/// work the same way on top of `wry`'s one-way `window.ipc.postMessage`
/// transport — injected into every page this hosts so every existing
/// Facet's own JS (which all already call `window.cefQuery(...)`, see
/// `terminal-xterm`'s `render_page`) works completely unmodified
/// regardless of whether CEF or `wry` ends up hosting it. Responses
/// come back via `evaluate_script` calling `__rashomonResolve` (see
/// `FacetWebView::build`), correlated by a request id since `wry`'s IPC
/// handler has no built-in reply mechanism the way `cefQuery` does.
const CEF_QUERY_SHIM_JS: &str = r#"
(function () {
  window.__rashomonPending = {};
  window.__rashomonNextId = 0;
  window.__rashomonResolve = function (id, response) {
    var cb = window.__rashomonPending[id];
    delete window.__rashomonPending[id];
    if (cb) cb(response);
  };
  window.cefQuery = function (opts) {
    var id = window.__rashomonNextId++;
    window.__rashomonPending[id] = opts.onSuccess || function () {};
    window.ipc.postMessage(JSON.stringify({ id: id, request: opts.request }));
  };
})();
"#;

/// Hosts a Facet View's rendered HTML (or the sidebar's) in a
/// transparent native webview (`wry`, WKWebView under the hood)
/// instead of an opaque CEF classic-embedded browser — see
/// [`NativeWindowHandle`]'s doc comment for why. Used for the sidebar
/// and the non-browser Facet tabs (terminal-xterm/graph-view/
/// extensions); real browser tabs stay on a real CEF `Browser`, where
/// opacity is expected anyway.
pub(crate) struct FacetWebView {
    webview: Arc<std::sync::OnceLock<wry::WebView>>,
}

impl FacetWebView {
    /// `html` is the Facet's already-rendered page, *before*
    /// `window.__rashomonWindowId` injection and the transparent-
    /// background override — both applied here (see
    /// `crate::inject_window_id`/`crate::inject_transparent_background`),
    /// same as the CEF path does for real browser tabs' window id.
    /// `bridge` is the same `InputBridge` CEF's `InputQueryHandler`
    /// dispatches through, via [`crate::dispatch_facet_request`] — one
    /// dispatch path regardless of rendering backend.
    pub(crate) fn new(
        parent: &NativeWindowHandle,
        window_id: &str,
        html: &str,
        bounds: wry::Rect,
        title: Arc<Mutex<String>>,
        bridge: Arc<Mutex<crate::InputBridge>>,
    ) -> anyhow::Result<Self> {
        let full_html = crate::inject_transparent_background(&crate::inject_window_id(html, window_id));
        let cell: Arc<std::sync::OnceLock<wry::WebView>> = Arc::new(std::sync::OnceLock::new());
        let cell_for_ipc = cell.clone();

        let webview = wry::WebViewBuilder::new()
            .with_bounds(bounds)
            .with_transparent(true)
            .with_initialization_script(CEF_QUERY_SHIM_JS)
            .with_html(full_html)
            .with_document_title_changed_handler(move |new_title| {
                *title.lock().expect("title lock poisoned") = new_title;
            })
            .with_ipc_handler(move |request: wry::http::Request<String>| {
                let Some(webview) = cell_for_ipc.get() else { return };
                let Ok(parsed) = serde_json::from_str::<serde_json::Value>(request.body()) else { return };
                let Some(id) = parsed.get("id").and_then(|v| v.as_i64()) else { return };
                let Some(req) = parsed.get("request").and_then(|v| v.as_str()) else { return };
                let response = crate::dispatch_facet_request(&bridge, req).unwrap_or_else(|e| e);
                let response_json = serde_json::to_string(&response).unwrap_or_default();
                let _ = webview.evaluate_script(&format!("window.__rashomonResolve({id}, {response_json})"));
            })
            .build_as_child(parent)
            .map_err(|e| anyhow::anyhow!("wry webview creation failed: {e}"))?;

        let _ = cell.set(webview);
        Ok(Self { webview: cell })
    }

    fn webview(&self) -> &wry::WebView {
        self.webview.get().expect("webview set immediately after construction, before this is ever called")
    }

    pub(crate) fn set_bounds(&self, bounds: wry::Rect) {
        if let Err(e) = self.webview().set_bounds(bounds) {
            eprintln!("FacetWebView::set_bounds failed: {e}");
        }
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        if let Err(e) = self.webview().set_visible(visible) {
            eprintln!("FacetWebView::set_visible failed: {e}");
        }
    }
}

// `wry::WebView` isn't `Send`/`Sync` by auto-trait inference (some
// internal delegate state uses `RefCell`/non-`Sync` Objective-C
// wrapper types) — asserted manually here under the same invariant as
// `NativeWindowHandle`: everything touching it stays on CEF/winit's
// one UI thread, synchronized through `Arc<Mutex<_>>` at the
// `BrowserSwitcherState` level. Required for `BrowserSwitcherState`
// (and so `KernelState`/`InputBridge`) to satisfy
// `cef::wrapper::message_router::BrowserSideHandler: Send + Sync`.
unsafe impl Send for FacetWebView {}
unsafe impl Sync for FacetWebView {}

/// Host browser-chrome, not a Facet — the address bar every browser
/// has, native `wry`-hosted (not a CEF Views `Textfield`, which no
/// longer exists in this architecture) so it can sit as a sibling of
/// the sidebar/content in the one native window. Its own tiny
/// request/response-free IPC protocol (just "navigate to this string"
/// on Enter) rather than the `cefQuery` shim — this is host UI, not a
/// Facet's page, so there's no existing JS depending on `cefQuery`
/// being present here at all.
pub(crate) struct UrlBarWebView {
    webview: wry::WebView,
}

const URLBAR_HTML: &str = r#"<!doctype html>
<html><body style="margin:0; background: transparent;">
<input id="url" spellcheck="false" style="box-sizing: border-box; width: 100%; height: 100%;
  border: none; outline: none; background: rgba(20,20,20,0.35); color: #eee; font-size: 13px;
  padding: 0 10px; border-radius: 6px;" placeholder="Enter a URL" />
<script>
document.getElementById('url').addEventListener('keydown', function (e) {
  if (e.key === 'Enter') {
    window.ipc.postMessage(e.target.value);
  }
});
</script>
</body></html>"#;

impl UrlBarWebView {
    pub(crate) fn new(
        parent: &NativeWindowHandle,
        bounds: wry::Rect,
        on_navigate: impl Fn(String) + 'static,
    ) -> anyhow::Result<Self> {
        let webview = wry::WebViewBuilder::new()
            .with_bounds(bounds)
            .with_transparent(true)
            .with_html(URLBAR_HTML)
            .with_ipc_handler(move |request: wry::http::Request<String>| {
                on_navigate(request.body().clone());
            })
            .build_as_child(parent)
            .map_err(|e| anyhow::anyhow!("urlbar webview creation failed: {e}"))?;
        Ok(Self { webview })
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        if let Err(e) = self.webview.set_visible(visible) {
            eprintln!("UrlBarWebView::set_visible failed: {e}");
        }
    }

    pub(crate) fn set_bounds(&self, bounds: wry::Rect) {
        if let Err(e) = self.webview.set_bounds(bounds) {
            eprintln!("UrlBarWebView::set_bounds failed: {e}");
        }
    }

    pub(crate) fn set_text(&self, text: &str) {
        let encoded = serde_json::to_string(text).unwrap_or_default();
        let _ = self
            .webview
            .evaluate_script(&format!("document.getElementById('url').value = {encoded};"));
    }
}

unsafe impl Send for UrlBarWebView {}
unsafe impl Sync for UrlBarWebView {}
