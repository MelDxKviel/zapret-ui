//! Restore the existing window when Finder or the Dock reopens this app.
//!
//! AppKit's public Apple-event handler API lets us handle `aevt/rapp` without
//! replacing the NSApplication delegate owned by Slint/winit. No polling or
//! Objective-C method replacement is needed.

use objc2::{define_class, msg_send, rc::Retained, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::NSApplicationWillFinishLaunchingNotification;
use objc2_foundation::{
    MainThreadMarker, NSAppleEventDescriptor, NSAppleEventManager, NSNotification,
    NSNotificationCenter, NSObject, NSObjectProtocol,
};

// Public CoreServices constants, expressed as their documented four-byte codes.
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const REOPEN_APPLICATION: u32 = u32::from_be_bytes(*b"rapp");

struct ReopenIvars {
    handler: Box<dyn Fn()>,
}

define_class!(
    // SAFETY: NSObject has no additional subclassing requirements. The class
    // is main-thread-only and retains its Rust callback in initialized ivars.
    #[unsafe(super = NSObject)]
    #[name = "ZapretUIReopenHandler"]
    #[thread_kind = MainThreadOnly]
    #[ivars = ReopenIvars]
    struct ReopenTarget;

    // SAFETY: NSObjectProtocol has no additional safety requirements.
    unsafe impl NSObjectProtocol for ReopenTarget {}

    impl ReopenTarget {
        // SAFETY: This matches the signature documented by NSAppleEventManager.
        // AppKit dispatches the application's reopen events on the main thread.
        #[unsafe(method(handleReopen:withReplyEvent:))]
        fn handle_reopen(&self, _event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            (self.ivars().handler)();
        }

        // SAFETY: NSNotificationCenter selectors take one notification object.
        // AppKit posts its launch notification synchronously on the main thread.
        #[unsafe(method(applicationWillFinishLaunching:))]
        fn application_will_finish_launching(&self, _notification: &NSNotification) {
            self.register();
            // Launch happens once. Stop observing as soon as registration has
            // happened after AppKit installed its default Apple-event handlers.
            unsafe { NSNotificationCenter::defaultCenter().removeObserver(self) };
        }
    }
);

impl ReopenTarget {
    fn new(mtm: MainThreadMarker, handler: impl Fn() + 'static) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ReopenIvars {
            handler: Box::new(handler),
        });
        // SAFETY: NSObject's init method has this signature and our ivars are set.
        unsafe { msg_send![super(this), init] }
    }

    fn register(&self) {
        // SAFETY: The target implements this selector with the exact documented
        // two-descriptor signature and the guard retains it until unregistering.
        // AEEventClass/AEEventID are UInt32 in the SDK. Calling the public method
        // directly avoids adding all CoreServices bindings for these two codes.
        unsafe {
            let manager = NSAppleEventManager::sharedAppleEventManager();
            let _: () = msg_send![
                &manager,
                setEventHandler: self,
                andSelector: sel!(handleReopen:withReplyEvent:),
                forEventClass: CORE_EVENT_CLASS,
                andEventID: REOPEN_APPLICATION,
            ];
        }
    }
}

/// Keep this guard alive for the complete UI event loop. Install it once, after
/// creating the Slint window, on the main thread. The callback runs on that thread.
///
/// Registration is immediate for an already running application and is repeated
/// at AppKit's recommended `willFinishLaunching` point for a fresh launch.
#[must_use = "The guard keeps the Dock/Finder reopen handler registered"]
pub struct ReopenGuard {
    target: Retained<ReopenTarget>,
}

pub fn install(handler: impl Fn() + 'static) -> anyhow::Result<ReopenGuard> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("Dock reopen handler requires the main thread"))?;
    let target = ReopenTarget::new(mtm, handler);
    // SAFETY: This is a public AppKit notification constant. The selector accepts
    // an NSNotification, and the main-thread target lives until guard teardown.
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            &target,
            sel!(applicationWillFinishLaunching:),
            Some(NSApplicationWillFinishLaunchingNotification),
            None,
        );
    }
    target.register();
    Ok(ReopenGuard { target })
}

impl Drop for ReopenGuard {
    fn drop(&mut self) {
        // SAFETY: This is the same retained target registered by install(). Its
        // main-thread-only type also keeps registration and teardown on the UI thread.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&self.target) };
        // SAFETY: This public method accepts two UInt32 event codes and returns void.
        unsafe {
            let manager = NSAppleEventManager::sharedAppleEventManager();
            let _: () = msg_send![
                &manager,
                removeEventHandlerForEventClass: CORE_EVENT_CLASS,
                andEventID: REOPEN_APPLICATION,
            ];
        }
    }
}
