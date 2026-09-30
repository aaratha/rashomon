//! macOS needs `NSApplication`/`NSApplicationDelegate` to conform to
//! CEF's Objective-C protocols for event handling to work at all — this
//! is the same setup validated in `crates/cef-spike`, just renamed.

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApp, NSApplication, NSApplicationDelegate, NSEvent};
use std::cell::Cell;

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
