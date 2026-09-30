//! macOS needs `NSApplication`/`NSApplicationDelegate` to conform to
//! CEF's Objective-C protocols for event handling to work at all — this
//! is a simplified port of `cefsimple`'s `mac.rs`, dropping the
//! `MainMenu` nib loading (we don't bundle that resource for this
//! spike) since a missing menu bar shouldn't block a browser window
//! from showing.

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApp, NSApplication, NSApplicationDelegate, NSEvent};
use std::cell::Cell;

define_class! {
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct SpikeAppDelegate;

    impl SpikeAppDelegate {
        #[unsafe(method(createApplication:))]
        unsafe fn create_application(&self, _object: Option<&AnyObject>) {
            let app = NSApp(MainThreadMarker::new().expect("not on main thread"));
            assert!(app.isKindOfClass(SpikeApplication::class()));
        }
    }

    unsafe impl NSObjectProtocol for SpikeAppDelegate {}

    unsafe impl NSApplicationDelegate for SpikeAppDelegate {}
}

impl SpikeAppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = SpikeAppDelegate::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Default)]
pub struct SpikeApplicationIvars {
    handling_send_event: Cell<Bool>,
}

define_class!(
    #[unsafe(super(NSApplication))]
    #[ivars = SpikeApplicationIvars]
    pub struct SpikeApplication;

    impl SpikeApplication {
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

    unsafe impl CrAppControlProtocol for SpikeApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        unsafe fn _set_handling_send_event(&self, handling_send_event: Bool) {
            self.ivars().handling_send_event.set(handling_send_event);
        }
    }

    unsafe impl CrAppProtocol for SpikeApplication {
        #[unsafe(method(isHandlingSendEvent))]
        unsafe fn _is_handling_send_event(&self) -> Bool {
            self.ivars().handling_send_event.get()
        }
    }

    unsafe impl CefAppProtocol for SpikeApplication {}
);

impl SpikeApplication {
    fn set_handling_send_event(&self, handling_send_event: bool) {
        unsafe { msg_send![self, setHandlingSendEvent: handling_send_event] }
    }

    fn is_handling_send_event(&self) -> bool {
        unsafe { msg_send![self, isHandlingSendEvent] }
    }
}

pub fn setup_spike_application() {
    let mtm = MainThreadMarker::new().expect("not on main thread");
    let app: Retained<NSApplication> = unsafe { msg_send![SpikeApplication::class(), sharedApplication] };
    let _ = mtm;
    assert!(app.isKindOfClass(SpikeApplication::class()));
}

pub fn setup_spike_app_delegate() -> Retained<SpikeAppDelegate> {
    let mtm = MainThreadMarker::new().expect("not on main thread");
    let delegate = SpikeAppDelegate::new(mtm);
    let app = NSApp(mtm);
    let proto = objc2::runtime::ProtocolObject::from_ref(&*delegate);
    app.setDelegate(Some(proto));

    unsafe {
        let _: () = msg_send![&delegate, performSelectorOnMainThread: sel!(createApplication:), withObject: std::ptr::null::<AnyObject>(), waitUntilDone: false];
    }

    delegate
}
