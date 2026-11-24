//! The WKWebView behind a web preview tab: an AppKit view stacked above GPUI's
//! own view in the window's content view and positioned from GPUI paint bounds.

use std::cell::Cell;

use anyhow::{Context as _, Result, anyhow, bail};
use block2::RcBlock;
use futures::channel::{mpsc, oneshot};
use gpui::{Bounds, Pixels, Window};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass as _, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBitmapImageFileType, NSBitmapImageRep, NSEvent,
    NSEventModifierFlags, NSEventType, NSImage, NSStandardKeyBindingResponding as _, NSView,
    NSWindow,
};
use objc2_foundation::{
    NSDictionary, NSError, NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSSize,
    NSString, NSURL, NSURLRequest,
};
use objc2_web_kit::{
    WKContentWorld, WKScriptMessage, WKScriptMessageHandler, WKSnapshotConfiguration,
    WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebView,
    WKWebViewConfiguration,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{MESSAGE_HANDLER_NAME, PAGE_SCRIPT, appkit_frame};

struct HandlerIvars {
    sender: mpsc::UnboundedSender<String>,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and `MessageHandler`
    // does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = HandlerIvars]
    struct MessageHandler;

    unsafe impl NSObjectProtocol for MessageHandler {}

    unsafe impl WKScriptMessageHandler for MessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive_script_message(
            &self,
            _controller: &WKUserContentController,
            message: &WKScriptMessage,
        ) {
            // SAFETY: Called by WebKit on the main thread with a live message.
            let body = unsafe { message.body() };
            if let Ok(text) = body.downcast::<NSString>() {
                // A closed receiver only means the preview is gone.
                self.ivars().sender.unbounded_send(text.to_string()).ok();
            }
        }
    }
);

impl MessageHandler {
    fn new(sender: mpsc::UnboundedSender<String>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HandlerIvars { sender });
        // SAFETY: `NSObject`'s `init` is its designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

pub(crate) struct NativeWebView {
    /// What GPUI positions. Holds the page and, when docked, Web Inspector,
    /// which WebKit lays out within the page's superview.
    container: Retained<NSView>,
    view: Retained<WKWebView>,
    controller: Retained<WKUserContentController>,
    /// Bumped on every paint that positions the view; see [`PaintToken`].
    paint_generation: Cell<u64>,
}

/// Lives inside a mouse listener registered during the frame that last
/// positioned the view. GPUI carries listeners of cached (reused) subtrees
/// into the next frame and drops the rest, so this is dropped exactly when a
/// frame is drawn that no longer contains the preview: a hidden tab, a
/// closed pane, a different workspace. That frame is the moment to hide.
pub(crate) struct PaintToken {
    native: std::rc::Rc<NativeWebView>,
    generation: u64,
}

impl PaintToken {
    pub(crate) fn release_keyboard(&self) {
        self.native.release_keyboard();
    }
}

impl Drop for PaintToken {
    fn drop(&mut self) {
        if self.native.paint_generation.get() == self.generation {
            self.native.hide();
        }
    }
}

fn main_thread() -> Result<MainThreadMarker> {
    MainThreadMarker::new().context("web preview used off the main thread")
}

fn gpui_view(window: &Window) -> Result<Retained<NSView>> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow!("no native window handle: {error}"))?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        bail!("the window is not an AppKit window");
    };
    // SAFETY: GPUI's macOS window hands out its live GPUIView, an NSView.
    unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
        .context("GPUI view was null")
}

impl NativeWebView {
    pub(crate) fn new(messages: mpsc::UnboundedSender<String>) -> Result<Self> {
        let mtm = main_thread()?;
        // SAFETY: All calls below are plain WebKit construction on the main thread.
        unsafe {
            let configuration = WKWebViewConfiguration::new(mtm);
            let controller = configuration.userContentController();
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &NSString::from_str(PAGE_SCRIPT),
                WKUserScriptInjectionTime::AtDocumentStart,
                true,
            );
            controller.addUserScript(&script);
            let handler = MessageHandler::new(messages, mtm);
            controller.addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*handler),
                &NSString::from_str(MESSAGE_HANDLER_NAME),
            );
            let view = WKWebView::initWithFrame_configuration(
                WKWebView::alloc(mtm),
                NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.)),
                &configuration,
            );
            view.setInspectable(true);
            view.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            let container = NSView::initWithFrame(
                NSView::alloc(mtm),
                NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.)),
            );
            container.addSubview(&view);
            container.setHidden(true);
            // Adds "Inspect Element" to the page's context menu. Private, but
            // WebKit has kept it stable for years; skipped if it ever goes.
            let preferences: Retained<NSObject> = msg_send![&*configuration, preferences];
            if preferences.respondsToSelector(sel!(_setDeveloperExtrasEnabled:)) {
                let () = msg_send![&*preferences, _setDeveloperExtrasEnabled: true];
            }
            Ok(Self {
                container,
                view,
                controller,
                paint_generation: Cell::new(0),
            })
        }
    }

    pub(crate) fn load(&self, url: &str) -> Result<()> {
        let url = NSURL::URLWithString(&NSString::from_str(url))
            .with_context(|| format!("`{url}` is not a valid URL"))?;
        let request = NSURLRequest::requestWithURL(&url);
        // SAFETY: The request is a valid NSURLRequest; called on the main thread.
        unsafe { self.view.loadRequest(&request) };
        Ok(())
    }

    pub(crate) fn reload(&self) {
        // SAFETY: Plain WebKit call on the main thread.
        unsafe { self.view.reload() };
    }

    pub(crate) fn go_back(&self) {
        // SAFETY: Plain WebKit call on the main thread.
        unsafe { self.view.goBack() };
    }

    pub(crate) fn go_forward(&self) {
        // SAFETY: Plain WebKit call on the main thread.
        unsafe { self.view.goForward() };
    }

    /// Opens Web Inspector docked under the page, inside the preview, or
    /// closes it. WKWebView has no public API for this; `_inspector` is what
    /// Safari's Develop menu uses.
    pub(crate) fn toggle_inspector(&self) -> Result<()> {
        if !self.view.respondsToSelector(sel!(_inspector)) {
            bail!("this WebKit version does not expose Web Inspector");
        }
        // SAFETY: `_inspector` returns the view's `_WKInspector`, whose
        // `isVisible`, `show`, `attach` and `close` take and return nothing
        // unusual; called on the main thread.
        unsafe {
            let inspector: Option<Retained<AnyObject>> = msg_send![&*self.view, _inspector];
            let inspector = inspector.context("the page has no inspector")?;
            let visible: bool = msg_send![&*inspector, isVisible];
            if visible {
                let () = msg_send![&*inspector, close];
            } else {
                let () = msg_send![&*inspector, show];
                let () = msg_send![&*inspector, attach];
            }
        }
        Ok(())
    }

    pub(crate) fn url(&self) -> Option<String> {
        // SAFETY: Plain WebKit call on the main thread.
        unsafe { self.view.URL() }
            .and_then(|url| url.absoluteString())
            .map(|url| url.to_string())
    }

    pub(crate) fn is_visible(&self) -> bool {
        !self.container.isHidden() && self.container.window().is_some()
    }

    /// Places the view over `bounds` (GPUI window coordinates) and returns
    /// the token whose drop hides it again; see [`PaintToken`].
    pub(crate) fn show(
        self: &std::rc::Rc<Self>,
        bounds: Bounds<Pixels>,
        window: &Window,
    ) -> Result<PaintToken> {
        let gpui_view = gpui_view(window)?;
        // SAFETY: Reading the superview of a live view on the main thread.
        let content_view =
            unsafe { gpui_view.superview() }.context("GPUI view has no superview")?;
        let in_this_window = unsafe { self.container.superview() }
            .is_some_and(|superview| std::ptr::eq(&*superview, &*content_view));
        if !in_this_window {
            self.container.removeFromSuperview();
            content_view.addSubview(&self.container);
        }
        let container = content_view.bounds();
        let frame = appkit_frame(bounds, container.size.height, content_view.isFlipped());
        self.container.setFrame(NSRect::new(
            NSPoint::new(frame.x, frame.y),
            NSSize::new(frame.width, frame.height),
        ));
        self.container.setHidden(false);
        let generation = self.paint_generation.get() + 1;
        self.paint_generation.set(generation);
        Ok(PaintToken {
            native: self.clone(),
            generation,
        })
    }

    pub(crate) fn hide(&self) {
        if self.container.isHidden() {
            return;
        }
        self.release_keyboard();
        self.container.setHidden(true);
        // A later paint must not mistake an older token for the current one.
        self.paint_generation.set(self.paint_generation.get() + 1);
    }

    pub(crate) fn has_keyboard(&self) -> bool {
        let Some(window) = self.view.window() else {
            return false;
        };
        window.firstResponder().is_some_and(|responder| {
            responder
                .downcast::<NSView>()
                .is_ok_and(|view| view.isDescendantOf(&self.container))
        })
    }

    pub(crate) fn take_keyboard(&self) {
        if let Some(window) = self.view.window()
            && !self.container.isHidden()
            && !self.has_keyboard()
        {
            window.makeFirstResponder(Some(&self.view));
        }
    }

    /// Hands AppKit key events back to GPUI's view. GPUIView never asks for
    /// first responder itself, so clicks on GPUI would otherwise keep typing
    /// into the page.
    pub(crate) fn release_keyboard(&self) {
        if !self.has_keyboard() {
            return;
        }
        let Some(window) = self.view.window() else {
            return;
        };
        // SAFETY: Reading the superview of a live view on the main thread.
        let gpui_view = unsafe { self.container.superview() }.and_then(|content_view| {
            content_view
                .subviews()
                .iter()
                .find(|view| !view.isDescendantOf(&self.container) && is_gpui_view(view))
        });
        window.makeFirstResponder(gpui_view.as_deref().map(|view| view.as_ref()));
    }

    /// Runs `body` as the body of an async function in the page, so it can
    /// `await`, and resolves with what it returns, which must be a string.
    pub(crate) fn evaluate(&self, body: &str) -> oneshot::Receiver<Result<String>> {
        let (sender, receiver) = oneshot::channel();
        let mtm = match main_thread() {
            Ok(mtm) => mtm,
            Err(error) => {
                sender.send(Err(error)).ok();
                return receiver;
            }
        };
        let sender = Cell::new(Some(sender));
        let block = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
            let Some(sender) = sender.take() else {
                return;
            };
            // SAFETY: WebKit passes either a live NSError or null.
            let outcome = match unsafe { error.as_ref() } {
                Some(error) => Err(anyhow!("{}", error.localizedDescription())),
                // SAFETY: WebKit passes either a live result object or null.
                None => match unsafe { result.as_ref() } {
                    Some(result) => result
                        .downcast_ref::<NSString>()
                        .map(|text| text.to_string())
                        .context("script did not return a string"),
                    None => Err(anyhow!("script returned nothing")),
                },
            };
            sender.send(outcome).ok();
        });
        // SAFETY: The block matches WebKit's completion handler signature;
        // called on the main thread.
        unsafe {
            self.view
                .callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                    &NSString::from_str(body),
                    None,
                    None,
                    &WKContentWorld::pageWorld(mtm),
                    Some(&block),
                )
        };
        receiver
    }

    /// PNG of what the page currently shows, at the screen's pixel density.
    pub(crate) fn snapshot(&self) -> oneshot::Receiver<Result<Vec<u8>>> {
        let (sender, receiver) = oneshot::channel();
        let mtm = match main_thread() {
            Ok(mtm) => mtm,
            Err(error) => {
                sender.send(Err(error)).ok();
                return receiver;
            }
        };
        let sender = Cell::new(Some(sender));
        let block = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            let Some(sender) = sender.take() else {
                return;
            };
            // SAFETY: WebKit passes either a live NSError or null.
            let outcome = match unsafe { error.as_ref() } {
                Some(error) => Err(anyhow!("{}", error.localizedDescription())),
                // SAFETY: WebKit passes either a live NSImage or null.
                None => unsafe { image.as_ref() }
                    .context("WebKit returned no snapshot")
                    .and_then(png_bytes),
            };
            sender.send(outcome).ok();
        });
        // SAFETY: Plain WebKit calls on the main thread; the block matches
        // WebKit's completion handler signature.
        unsafe {
            let configuration = WKSnapshotConfiguration::new(mtm);
            self.view
                .takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &block);
        }
        receiver
    }

    /// Clicks at `point`, in CSS pixels from the page viewport's top-left, by
    /// sending real mouse events through the window, so the page sees a
    /// trusted click exactly as if the user made it. Keyboard focus is handed
    /// back to whoever had it, so an agent clicking never steals the user's
    /// typing.
    pub(crate) fn click(&self, x: f64, y: f64) -> Result<()> {
        let window = self.on_screen_window()?;
        let bounds = self.view.bounds();
        // SAFETY: Plain WebKit call on the main thread.
        let zoom = unsafe { self.view.pageZoom() };
        let (x, y) = (x * zoom, y * zoom);
        if !(0.0..bounds.size.width).contains(&x) || !(0.0..bounds.size.height).contains(&y) {
            bail!("the element's center ({x}, {y}) is outside the visible page");
        }
        let local = if self.view.isFlipped() {
            NSPoint::new(x, y)
        } else {
            NSPoint::new(x, bounds.size.height - y)
        };
        let in_window = self.view.convertPoint_toView(local, None);
        let content_view = window.contentView().context("window has no content view")?;
        let target = content_view.hitTest(in_window);
        if !target.is_some_and(|target| target.isDescendantOf(&self.view)) {
            bail!("something is covering the preview at that point");
        }
        let previous_responder = window.firstResponder();
        let timestamp = NSProcessInfo::processInfo().systemUptime();
        for (event_type, offset) in [
            (NSEventType::LeftMouseDown, 0.0),
            (NSEventType::LeftMouseUp, 0.05),
        ] {
            let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                event_type,
                in_window,
                NSEventModifierFlags::empty(),
                timestamp + offset,
                window.windowNumber(),
                None,
                0,
                1,
                1.0,
            )
            .context("AppKit refused to create the mouse event")?;
            window.sendEvent(&event);
        }
        window.makeFirstResponder(previous_responder.as_deref());
        Ok(())
    }
}

impl NativeWebView {
    fn on_screen_window(&self) -> Result<Retained<NSWindow>> {
        if self.container.isHidden() {
            bail!("the preview is not on screen (another tab or a modal is in front of it)");
        }
        self.view.window().context("the preview is not in a window")
    }

    /// Types `text` into the page's focused element as real text input, then
    /// presses Return if `submit`. Like [`Self::click`], keyboard focus goes
    /// back to whoever had it.
    pub(crate) fn type_text(&self, text: &str, submit: bool) -> Result<()> {
        let window = self.on_screen_window()?;
        let previous_responder = window.firstResponder();
        window.makeFirstResponder(Some(&self.view));
        // SAFETY: WKWebView takes an NSString here; called on the main thread.
        unsafe { self.view.insertText(&NSString::from_str(text)) };
        let result = if submit {
            self.press_return(&window)
        } else {
            Ok(())
        };
        window.makeFirstResponder(previous_responder.as_deref());
        result
    }

    fn press_return(&self, window: &NSWindow) -> Result<()> {
        const RETURN_KEY_CODE: u16 = 36;
        let timestamp = NSProcessInfo::processInfo().systemUptime();
        let characters = NSString::from_str("\r");
        for (event_type, offset) in [(NSEventType::KeyDown, 0.0), (NSEventType::KeyUp, 0.02)] {
            let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                event_type,
                NSPoint::new(0., 0.),
                NSEventModifierFlags::empty(),
                timestamp + offset,
                window.windowNumber(),
                None,
                &characters,
                &characters,
                false,
                RETURN_KEY_CODE,
            )
            .context("AppKit refused to create the key event")?;
            window.sendEvent(&event);
        }
        Ok(())
    }
}

impl Drop for NativeWebView {
    fn drop(&mut self) {
        // SAFETY: Plain WebKit calls on the main thread.
        unsafe {
            self.controller
                .removeScriptMessageHandlerForName(&NSString::from_str(MESSAGE_HANDLER_NAME));
        }
        self.container.removeFromSuperview();
    }
}

fn is_gpui_view(view: &NSView) -> bool {
    view.class()
        .name()
        .to_str()
        .is_ok_and(|name| name == "GPUIView")
}

fn png_bytes(image: &NSImage) -> Result<Vec<u8>> {
    let tiff = image
        .TIFFRepresentation()
        .context("snapshot has no bitmap")?;
    let bitmap = NSBitmapImageRep::imageRepWithData(&tiff).context("snapshot is not a bitmap")?;
    // SAFETY: An empty properties dictionary is valid for PNG encoding.
    let png = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .context("encoding the snapshot as PNG")?;
    Ok(png.to_vec())
}
