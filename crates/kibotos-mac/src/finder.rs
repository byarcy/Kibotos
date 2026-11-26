//! Files Finder asks us to open: double click, "Open With", or a drop on
//! the Dock icon. macOS sends them as an "open documents" Apple event
//! (`kAEOpenDocuments`). Cocoa installs its own handler while launching and
//! delivers the documents of a launch right after
//! `NSApplicationWillFinishLaunchingNotification`, before the window exists,
//! so we replace the handler when that notification arrives (Apple's advice)
//! and queue the paths for the UI, which picks them up on its first frame.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use eframe::egui;
use objc2::rc::Retained;
use objc2::{AnyThread, define_class, msg_send, sel};
use objc2_foundation::{
    NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSNotificationCenter, NSObject, NSString,
};

static PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static CTX: OnceLock<egui::Context> = OnceLock::new();

/// Four-character Apple event codes.
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

fn path_of(item: &NSAppleEventDescriptor) -> Option<PathBuf> {
    let url = item.fileURLValue()?;
    let path = url.path()?;
    Some(PathBuf::from(path.to_string()))
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "KibotosOpenDocumentsHandler"]
    struct OpenHandler;

    impl OpenHandler {
        #[unsafe(method(handleOpenDocuments:withReplyEvent:))]
        fn handle_open_documents(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            let mut paths = Vec::new();
            if let Some(list) = event.paramDescriptorForKeyword(DIRECT_OBJECT) {
                let count = list.numberOfItems();
                if count <= 0 {
                    paths.extend(path_of(&list));
                }
                for i in 1..=count {
                    if let Some(item) = list.descriptorAtIndex(i) {
                        paths.extend(path_of(&item));
                    }
                }
            }
            deliver(paths);
        }

        #[unsafe(method(applicationWillFinishLaunching:))]
        fn will_finish_launching(&self, _note: &NSNotification) {
            register(self);
        }
    }
);

fn deliver(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    if let Ok(mut p) = PENDING.lock() {
        p.extend(paths);
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

fn new_handler() -> Retained<OpenHandler> {
    let this = OpenHandler::alloc().set_ivars(());
    // SAFETY: plain NSObject initializer.
    unsafe { msg_send![super(this), init] }
}

fn register(handler: &OpenHandler) {
    let manager = NSAppleEventManager::sharedAppleEventManager();
    // SAFETY: the selector matches the method defined above, with the
    // (event, reply) signature the Apple event manager calls.
    unsafe {
        manager.setEventHandler_andSelector_forEventClass_andEventID(
            handler,
            sel!(handleOpenDocuments:withReplyEvent:),
            CORE_EVENT_CLASS,
            OPEN_DOCUMENTS,
        );
    }
}

/// Call at the start of `main`, before the app runs: takes over the open
/// documents event as soon as Cocoa has installed its own handler.
pub fn install_early() {
    let handler = new_handler();
    let name = NSString::from_str("NSApplicationWillFinishLaunchingNotification");
    // SAFETY: the selector matches `will_finish_launching`, which takes the
    // notification; the handler is never freed (see below).
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            &handler,
            sel!(applicationWillFinishLaunching:),
            Some(&name),
            None,
        );
    }
    // Neither the notification center nor the Apple event manager retains
    // the handler: keep it for the app's life.
    std::mem::forget(handler);
}

/// Called once the UI exists: lets new documents wake it up, and installs
/// the handler again in case anything replaced it during launch.
pub fn install(ctx: &egui::Context) {
    let _ = CTX.set(ctx.clone());
    let handler = new_handler();
    register(&handler);
    std::mem::forget(handler);
}

/// Paths received since the last call.
pub fn take() -> Vec<PathBuf> {
    PENDING.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default()
}
