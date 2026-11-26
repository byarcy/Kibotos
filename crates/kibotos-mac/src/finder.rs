//! Files Finder asks us to open: double click, "Open With", or a drop on
//! the Dock icon. macOS sends them as an "open documents" Apple event
//! (`kAEOpenDocuments`). We install our own handler for it once the app has
//! finished launching (Cocoa installs its default handlers before that, and
//! the documents of a launch arrive after it), and queue the paths for the
//! UI, which picks them up on its next frame.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use eframe::egui;
use objc2::rc::Retained;
use objc2::{AnyThread, define_class, msg_send, sel};
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSObject};

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

/// Installs the handler. Call once, on the main thread, after launch
/// (eframe's app creator runs from `applicationDidFinishLaunching`).
pub fn install(ctx: &egui::Context) {
    let _ = CTX.set(ctx.clone());
    let this = OpenHandler::alloc().set_ivars(());
    // SAFETY: plain NSObject initializer.
    let handler: Retained<OpenHandler> = unsafe { msg_send![super(this), init] };
    let manager = NSAppleEventManager::sharedAppleEventManager();
    // SAFETY: the selector matches the method defined above, with the
    // (event, reply) signature the Apple event manager calls.
    unsafe {
        manager.setEventHandler_andSelector_forEventClass_andEventID(
            &handler,
            sel!(handleOpenDocuments:withReplyEvent:),
            CORE_EVENT_CLASS,
            OPEN_DOCUMENTS,
        );
    }
    // The manager does not retain its handler: keep it for the app's life.
    std::mem::forget(handler);
}

/// Paths received since the last call.
pub fn take() -> Vec<PathBuf> {
    PENDING.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default()
}
