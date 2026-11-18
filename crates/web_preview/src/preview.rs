use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use editor::Editor;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{
    App, Context, DispatchPhase, Entity, EventEmitter, FocusHandle, Focusable, Image, ImageFormat,
    MouseDownEvent, Render, SharedString, Task, Window, canvas, img,
};
use ui::{IconButton, IconName, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::item::{Item, ItemEvent};
use workspace::notifications::NotifyResultExt as _;

use crate::native::NativeWebView;
use crate::{
    ConsoleBuffer, ConsoleEntry, DEFAULT_URL, Open, PageMessage, agent_tools, normalize_url,
};

/// Ignore the page's own focus report for this long after an agent click, so
/// the agent clicking does not pull keyboard focus away from the user.
const AGENT_CLICK_FOCUS_GRACE: Duration = Duration::from_millis(500);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            WebPreview::open(workspace, DEFAULT_URL, window, cx).notify_err(workspace, cx);
        });
        if let Some(window) = window {
            agent_tools::register(workspace, window, cx);
        }
    })
    .detach();
}

enum Overlay {
    None,
    /// A modal opened; the page stays live until its snapshot is ready.
    Capturing,
    /// The native view is hidden and this image is drawn in its place, so
    /// GPUI overlays can draw on top of it.
    Snapshot(Option<Arc<Image>>),
}

pub(crate) struct WebPreview {
    native: Rc<NativeWebView>,
    url_editor: Entity<Editor>,
    focus_handle: FocusHandle,
    title: Option<SharedString>,
    console: ConsoleBuffer,
    overlay: Overlay,
    load_waiters: Vec<oneshot::Sender<String>>,
    ignore_page_focus_until: Option<Instant>,
    _message_task: Task<()>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl WebPreview {
    pub(crate) fn open(
        workspace: &mut Workspace,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Result<Entity<Self>> {
        let modal_layer = workspace.modal_layer().clone();
        let (sender, receiver) = mpsc::unbounded();
        let native = Rc::new(NativeWebView::new(sender)?);
        let url = normalize_url(url).unwrap_or_else(|| DEFAULT_URL.to_string());
        native.load(&url)?;
        let preview = cx.new(|cx| {
            let url_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(url.as_str(), window, cx);
                editor
            });
            let focus_handle = cx.focus_handle();
            let subscriptions = vec![
                cx.on_focus(&focus_handle, window, |this: &mut Self, _, _| {
                    this.native.take_keyboard();
                }),
                cx.observe_in(
                    &modal_layer,
                    window,
                    |this: &mut Self, modal_layer, _, cx| {
                        let has_modal = modal_layer.read(cx).has_active_modal();
                        this.modal_changed(has_modal, cx);
                    },
                ),
            ];
            let message_task = cx.spawn_in(window, async move |this, cx| {
                let mut receiver = receiver;
                while let Some(message) = receiver.next().await {
                    let Some(message) = PageMessage::parse(&message) else {
                        continue;
                    };
                    if this
                        .update_in(cx, |this, window, cx| {
                            this.handle_message(message, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            Self {
                native,
                url_editor,
                focus_handle,
                title: None,
                console: ConsoleBuffer::default(),
                overlay: Overlay::None,
                load_waiters: Vec::new(),
                ignore_page_focus_until: None,
                _message_task: message_task,
                _subscriptions: subscriptions,
            }
        });
        workspace.add_item_to_active_pane(Box::new(preview.clone()), None, true, window, cx);
        Ok(preview)
    }

    fn handle_message(
        &mut self,
        message: PageMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match message {
            PageMessage::Console { level, text } => self.console.push(level, text),
            PageMessage::Focus => {
                let agent_clicked = self
                    .ignore_page_focus_until
                    .is_some_and(|until| Instant::now() < until);
                if !agent_clicked && !self.focus_handle.is_focused(window) {
                    window.focus(&self.focus_handle, cx);
                }
            }
            PageMessage::Loaded { url, title } => {
                let editing = self.url_editor.focus_handle(cx).is_focused(window);
                if !editing {
                    self.url_editor
                        .update(cx, |editor, cx| editor.set_text(url.as_str(), window, cx));
                }
                self.title = (!title.trim().is_empty()).then(|| title.trim().to_string().into());
                for waiter in self.load_waiters.drain(..) {
                    waiter.send(url.clone()).ok();
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }
        }
    }

    fn modal_changed(&mut self, has_modal: bool, cx: &mut Context<Self>) {
        if !has_modal {
            match std::mem::replace(&mut self.overlay, Overlay::None) {
                Overlay::None => {}
                Overlay::Snapshot(Some(image)) => {
                    // Every snapshot is a new image; drop it from GPUI's cache.
                    gpui::ImageSource::Image(image).remove_asset(cx);
                    cx.notify();
                }
                Overlay::Capturing | Overlay::Snapshot(None) => cx.notify(),
            }
            return;
        }
        if !matches!(self.overlay, Overlay::None) || !self.native.is_visible() {
            return;
        }
        self.overlay = Overlay::Capturing;
        let snapshot = self.native.snapshot();
        cx.spawn(async move |this, cx| {
            let image = match snapshot.await {
                Ok(Ok(png)) => Some(Arc::new(Image::from_bytes(ImageFormat::Png, png))),
                Ok(Err(error)) => {
                    log::error!("snapshotting the web preview: {error:#}");
                    None
                }
                Err(_) => None,
            };
            this.update(cx, |this, cx| {
                // The modal may have closed while WebKit was rendering.
                if matches!(this.overlay, Overlay::Capturing) {
                    this.overlay = Overlay::Snapshot(image);
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
    }

    pub(crate) fn navigate(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<String> {
        let url = normalize_url(url).ok_or_else(|| anyhow::anyhow!("empty URL"))?;
        self.native.load(&url)?;
        self.url_editor
            .update(cx, |editor, cx| editor.set_text(url.as_str(), window, cx));
        Ok(url)
    }

    /// Resolves with the URL once the next page reaches DOMContentLoaded.
    pub(crate) fn next_load(&mut self) -> oneshot::Receiver<String> {
        let (sender, receiver) = oneshot::channel();
        self.load_waiters.push(sender);
        receiver
    }

    pub(crate) fn native(&self) -> &Rc<NativeWebView> {
        &self.native
    }

    pub(crate) fn take_console(&mut self) -> (Vec<ConsoleEntry>, usize) {
        self.console.take()
    }

    pub(crate) fn ignore_page_focus_briefly(&mut self) {
        self.ignore_page_focus_until = Some(Instant::now() + AGENT_CLICK_FOCUS_GRACE);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.url_editor.read(cx).text(cx);
        if let Some(url) = self.navigate(&text, window, cx).log_err() {
            log::info!("web preview navigating to {url}");
        }
        window.focus(&self.focus_handle, cx);
    }
}

impl EventEmitter<ItemEvent> for WebPreview {}

impl Focusable for WebPreview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for WebPreview {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.title.clone().unwrap_or_else(|| "Preview".into())
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        self.native.url().map(Into::into)
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Public))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for WebPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.overlay {
            Overlay::Snapshot(Some(image)) => img(image.clone()).size_full().into_any_element(),
            Overlay::Snapshot(None) => div()
                .size_full()
                .bg(cx.theme().colors().editor_background)
                .into_any_element(),
            Overlay::None | Overlay::Capturing => {
                let native = self.native.clone();
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        let visible = bounds.intersect(&window.content_mask().bounds);
                        if visible.size.width <= px(0.) || visible.size.height <= px(0.) {
                            native.hide();
                            return;
                        }
                        let Some(token) = native.show(visible, window).log_err() else {
                            return;
                        };
                        // Any mouse down GPUI sees landed outside the page.
                        window.on_mouse_event(move |_: &MouseDownEvent, phase, _, _| {
                            if phase == DispatchPhase::Capture {
                                token.release_keyboard();
                            }
                        });
                    },
                )
                .size_full()
                .into_any_element()
            }
        };
        v_flex()
            .key_context("WebPreview")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        IconButton::new("reload", IconName::RotateCw)
                            .tooltip(Tooltip::text("Reload"))
                            .on_click(cx.listener(|this, _, _, _| this.native.reload())),
                    )
                    .child(div().flex_1().child(self.url_editor.clone())),
            )
            .child(div().flex_1().min_h_0().w_full().child(content))
    }
}
