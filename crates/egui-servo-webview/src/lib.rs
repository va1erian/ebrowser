//! `egui-servo-webview` – a reusable egui widget that embeds the Servo browser engine.
//!
//! # Quick start
//! ```no_run
//! # use egui_servo_webview::{WebViewConfig, WebViewHost, WebViewSource};
//! # use dpi::PhysicalSize;
//! // Once per window, in eframe::App::new():
//! // let host = WebViewHost::from_eframe(cc, PhysicalSize::new(1280, 720))?;
//! // let view = host.new_view(
//! //     &cc.egui_ctx,
//! //     WebViewConfig::new(WebViewSource::Url("https://servo.org".into())),
//! // );
//! //
//! // Every frame, in eframe::App::update():
//! // host.spin();                 // drive the engine once, whatever the view count
//! // let events = view.show(ui);  // draw, once per view
//! ```
//!
//! One [`WebViewHost`] owns the engine; it can produce any number of
//! [`WebView`]s that share it.

// Re-exported so callers can name the types in this crate's signatures without
// taking their own dependency on these crates (and risking a version skew).
pub use dpi;
pub use url;
pub use servo::{LoadStatus, WebResourceRequest, Image};

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use base64::{Engine as _, engine::general_purpose};
use dpi::PhysicalSize;
use euclid::Scale;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use url::Url;

use servo::{
    DevicePixel, DeviceVector2D, InputEvent, OffscreenRenderingContext, RenderingContext,
    Scroll, Servo, ServoBuilder, WebViewBuilder, WebViewDelegate,
    WebViewPoint, WebViewVector, WindowRenderingContext, NavigationRequest, WebResourceLoad,
    WebResourceResponse,
};
// `servo::WebView` is the engine-side view. Ours (below) wraps it, so alias the
// engine type to keep the two unambiguous at every use site.
use servo::WebView as ServoWebView;
use servo::input_events::{
    KeyboardEvent, MouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
};
use servo::DeviceIndependentPixel;
// keyboard_types is re-exported by servo. We import it separately to
// construct KeyboardEvent values – use fully-qualified paths to avoid
// conflicts with the servo::Key re-export.
use keyboard_types::{KeyState, Location, Modifiers};


// ─── Public API types ────────────────────────────────────────────────────────

/// What to load in the webview.
#[derive(Clone)]
pub enum WebViewSource {
    /// Navigate to a remote (or local) URL, e.g. `"https://example.com"`.
    Url(String),
    /// Render an in-memory HTML string. Always base64-encoded into a
    /// `data:` URL, so every relative link and resource in it is dead — use
    /// [`WebViewSource::HtmlWithBase`] when the HTML has any.
    Html(String),
    /// Render an in-memory HTML string with relative links and resources
    /// resolved against `base` (e.g. a mail message's `Content-Location`, or
    /// the sender's domain), via an injected `<base href>`.
    HtmlWithBase {
        /// The HTML to render.
        html: String,
        /// The base URL relative links/resources resolve against.
        base: String,
    },
}

/// Events emitted by [`WebView::show`].
#[derive(Debug, Clone)]
pub enum WebViewEvent {
    /// The user triggered a navigation to a new URL (link click, etc.) and the
    /// [`WebViewHandler`] denied it. The host typically opens this externally.
    LinkClicked(String),
    /// `WebViewDelegate::notify_url_changed`.
    UrlChanged(String),
    /// `WebViewDelegate::notify_page_title_changed`.
    TitleChanged(Option<String>),
    /// `WebViewDelegate::notify_status_text_changed`.
    StatusTextChanged(Option<String>),
    /// `WebViewDelegate::notify_load_status_changed`.
    LoadStatusChanged(LoadStatus),
    /// `WebViewDelegate::notify_favicon_changed`. Carries no payload upstream;
    /// re-read the favicon via [`WebView::favicon`].
    FaviconChanged,
    /// `WebViewDelegate::notify_history_changed`.
    HistoryChanged {
        /// The full back/forward list, oldest first.
        entries: Vec<String>,
        /// Index of the current entry within `entries`.
        current: usize,
    },
    /// `WebViewDelegate::notify_traversal_complete` — a `go_back`/`go_forward`
    /// finished.
    TraversalComplete,
}

/// Decision returned by [`WebViewHandler::navigation`] for a navigation past
/// the view's initial load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationPolicy {
    /// Let the navigation proceed in this view.
    Allow,
    /// Block it. The view stays where it is; a [`WebViewEvent::LinkClicked`]
    /// is queued so the host can act on it (open externally, etc.).
    Deny,
}

/// A response to serve in place of a real network fetch, returned from
/// [`WebViewHandler::intercept`] wrapped in [`InterceptOutcome::Serve`].
pub struct InterceptedResponse {
    /// HTTP status code, e.g. `200`.
    pub status_code: u16,
    /// The response body.
    pub body: Vec<u8>,
}

impl InterceptedResponse {
    /// A `200 OK` response with `body`.
    pub fn ok(body: Vec<u8>) -> Self {
        Self { status_code: 200, body }
    }
}

/// What [`WebViewHandler::intercept`] decided to do with one resource load.
pub enum InterceptOutcome {
    /// Let it load normally, from the network.
    Allow,
    /// Cancel it — the page sees a network error, the same as a blocked
    /// request in any other browser. This is the real "stop a remote image
    /// or stylesheet from loading" control point; markup alone cannot do
    /// this (removing an `<img src>` in the DOM doesn't un-issue a request
    /// already made, and Servo's `UserContentManager` has no CSP to forbid
    /// one either — see B5 in PLAN.md).
    Block,
    /// Serve `InterceptedResponse` instead of the network — for e.g. a
    /// `cid:` part resolved to bytes already in memory.
    Serve(InterceptedResponse),
}

/// Host-supplied policy for navigation and resource loading in a [`WebView`].
///
/// Both Servo hooks behind this trait **fail open**: an unhandled
/// [`NavigationRequest`] allows the navigation, and an unhandled
/// [`WebResourceLoad`] lets the resource load unmodified. The default
/// [`WebViewHandler::navigation`] below denies rather than relying on that, but
/// [`WebViewHandler::intercept`]'s default of [`InterceptOutcome::Allow`] is
/// the same "let it through" behaviour Servo would apply anyway — that one is
/// a real default, not a footgun, since choosing not to intercept a resource
/// is a legitimate, common answer.
pub trait WebViewHandler {
    /// Called for every navigation after the view's initial load (which is
    /// always allowed — otherwise the view could never load anything).
    /// Defaults to [`NavigationPolicy::Deny`], matching this crate's
    /// pre-A3 behaviour of reporting every subsequent navigation as a
    /// [`WebViewEvent::LinkClicked`] rather than navigating the view itself.
    fn navigation(&mut self, url: &Url) -> NavigationPolicy {
        let _ = url;
        NavigationPolicy::Deny
    }

    /// Called for every resource load the view makes. See [`InterceptOutcome`].
    fn intercept(&mut self, request: &WebResourceRequest) -> InterceptOutcome {
        let _ = request;
        InterceptOutcome::Allow
    }
}

/// The [`WebViewHandler`] used when a [`WebViewConfig`] does not supply one:
/// both methods run their documented defaults.
struct DefaultHandler;
impl WebViewHandler for DefaultHandler {}

// ─── Internal delegate ───────────────────────────────────────────────────────

struct Delegate {
    egui_ctx: egui::Context,
    /// Events queued during delegate callbacks; drained by `show()` each frame.
    events: Rc<RefCell<Vec<WebViewEvent>>>,
    /// Track whether the very first load has been dispatched so we can
    /// distinguish the initial navigation from user-initiated link clicks.
    initial_load_done: Rc<RefCell<bool>>,
    handler: Rc<RefCell<dyn WebViewHandler>>,
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _webview: ServoWebView) {
        self.egui_ctx.request_repaint();
    }

    fn request_navigation(&self, _webview: ServoWebView, request: NavigationRequest) {
        let mut done = self.initial_load_done.borrow_mut();
        if *done {
            // `deny()` is required, not optional, when the policy says so:
            // servo's `NavigationRequest` fails OPEN. Its `Drop` impl sends
            // "allow", so simply dropping the request here would let the
            // navigation proceed *and* emit a LinkClicked event, so a link
            // would open in the webview and in the system browser at once.
            // This already shipped as a bug once; see the A2 commit.
            match self.handler.borrow_mut().navigation(&request.url) {
                NavigationPolicy::Allow => request.allow(),
                NavigationPolicy::Deny => {
                    self.events
                        .borrow_mut()
                        .push(WebViewEvent::LinkClicked(request.url.to_string()));
                    request.deny();
                }
            }
        } else {
            *done = true;
            // The initial load is always allowed; there would otherwise be no
            // way to get anything on screen at all.
            request.allow();
        }
    }

    fn load_web_resource(&self, _webview: ServoWebView, load: WebResourceLoad) {
        match self.handler.borrow_mut().intercept(load.request()) {
            InterceptOutcome::Allow => {
                // Dropping `load` here sends `DoNotIntercept`, which is
                // exactly "let this load through unmodified" — the correct
                // outcome when the handler chose not to intercept, not a
                // footgun like the navigation fail-open above.
            }
            InterceptOutcome::Block => {
                // `intercept()` must be called before `cancel()` — there is
                // no "refuse without first intercepting" entry point in
                // Servo's API. The response passed in is never seen by the
                // page: cancelling turns this into a network error, not a
                // `200` with this body.
                let placeholder = WebResourceResponse::new(load.request().url.clone());
                load.intercept(placeholder).cancel();
            }
            InterceptOutcome::Serve(intercepted) => {
                let servo_response = WebResourceResponse::new(load.request().url.clone())
                    .status_code(
                        http::StatusCode::from_u16(intercepted.status_code)
                            .unwrap_or(http::StatusCode::OK),
                    );
                let mut in_flight = load.intercept(servo_response);
                in_flight.send_body_data(intercepted.body);
                in_flight.finish();
            }
        }
    }

    fn notify_url_changed(&self, _webview: ServoWebView, url: Url) {
        self.events.borrow_mut().push(WebViewEvent::UrlChanged(url.to_string()));
    }

    fn notify_page_title_changed(&self, _webview: ServoWebView, title: Option<String>) {
        self.events.borrow_mut().push(WebViewEvent::TitleChanged(title));
    }

    fn notify_status_text_changed(&self, _webview: ServoWebView, status: Option<String>) {
        self.events.borrow_mut().push(WebViewEvent::StatusTextChanged(status));
    }

    fn notify_load_status_changed(&self, _webview: ServoWebView, status: LoadStatus) {
        self.events.borrow_mut().push(WebViewEvent::LoadStatusChanged(status));
    }

    fn notify_favicon_changed(&self, _webview: ServoWebView) {
        self.events.borrow_mut().push(WebViewEvent::FaviconChanged);
    }

    fn notify_history_changed(&self, _webview: ServoWebView, entries: Vec<Url>, current: usize) {
        self.events.borrow_mut().push(WebViewEvent::HistoryChanged {
            entries: entries.into_iter().map(|u| u.to_string()).collect(),
            current,
        });
    }

    fn notify_traversal_complete(&self, _webview: ServoWebView, _id: servo::TraversalId) {
        self.events.borrow_mut().push(WebViewEvent::TraversalComplete);
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Something went wrong setting up the engine.
#[derive(Debug)]
pub enum WebViewError {
    /// The window or display handle could not be obtained from the host.
    Handle(raw_window_handle::HandleError),
    /// Servo could not create a rendering context for the window.
    RenderingContext(String),
}

impl fmt::Display for WebViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handle(e) => write!(f, "could not get a window/display handle: {e}"),
            Self::RenderingContext(e) => write!(f, "could not create a rendering context: {e}"),
        }
    }
}

impl std::error::Error for WebViewError {}

impl From<raw_window_handle::HandleError> for WebViewError {
    fn from(e: raw_window_handle::HandleError) -> Self {
        Self::Handle(e)
    }
}

// ─── WebViewHost ─────────────────────────────────────────────────────────────

/// Owns the Servo engine and the window's rendering context.
///
/// Create **one per window**, then create any number of [`WebView`]s from it
/// with [`WebViewHost::new_view`]. The engine is shared, so a second view costs
/// a rendering context rather than a second browser engine.
///
/// **Must live on the UI thread** — Servo's types are `!Send + !Sync`.
pub struct WebViewHost {
    servo: Servo,
    window_ctx: Rc<WindowRenderingContext>,
    /// Source of per-view ids, used to give each view a distinct egui texture.
    next_view_id: Cell<u64>,
}

impl WebViewHost {
    /// Create the engine and bind it to a window.
    ///
    /// `size` is the window's size in physical pixels; individual views are
    /// sized independently when they are shown.
    pub fn new(
        display: &impl HasDisplayHandle,
        window: &impl HasWindowHandle,
        size: PhysicalSize<u32>,
    ) -> Result<Self, WebViewError> {
        let servo = ServoBuilder::default().build();

        let window_ctx = WindowRenderingContext::new(
            display.display_handle()?,
            window.window_handle()?,
            size,
        )
        .map_err(|e| WebViewError::RenderingContext(format!("{e:?}")))?;

        Ok(Self {
            servo,
            window_ctx: Rc::new(window_ctx),
            next_view_id: Cell::new(0),
        })
    }

    /// Convenience constructor for eframe applications.
    ///
    /// Equivalent to [`WebViewHost::new`] with the handles eframe's
    /// [`eframe::CreationContext`] provides.
    #[cfg(feature = "eframe")]
    pub fn from_eframe(
        cc: &eframe::CreationContext<'_>,
        size: PhysicalSize<u32>,
    ) -> Result<Self, WebViewError> {
        Self::new(cc, cc, size)
    }

    /// Create a new view. The engine is shared with every other view.
    pub fn new_view(&self, egui_ctx: &egui::Context, config: WebViewConfig) -> WebView {
        let view_id = self.next_view_id.get();
        self.next_view_id.set(view_id + 1);

        let offscreen_ctx = Rc::new(self.window_ctx.offscreen_context(config.size));

        let events: Rc<RefCell<Vec<WebViewEvent>>> = Rc::new(RefCell::new(Vec::new()));
        let initial_load_done: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let handler = config
            .handler
            .clone()
            .unwrap_or_else(|| Rc::new(RefCell::new(DefaultHandler)));

        let delegate = Rc::new(Delegate {
            egui_ctx: egui_ctx.clone(),
            events: events.clone(),
            initial_load_done,
            handler,
        });

        let servo_view = WebViewBuilder::new(
            &self.servo,
            offscreen_ctx.clone() as Rc<dyn RenderingContext>,
        )
        .url(WebView::source_to_url(&config.source))
        .delegate(delegate)
        .build();

        WebView {
            servo_view,
            offscreen_ctx,
            events,
            texture: None,
            texture_name: format!("egui_servo_webview_{view_id}"),
            last_phys_size: config.size,
            last_mouse_pos: None,
        }
    }

    /// Drive the engine. Call **once per frame**, regardless of how many views
    /// exist — this is why views no longer spin the loop themselves.
    pub fn spin(&self) {
        self.servo.spin_event_loop();
    }
}

// ─── WebView ─────────────────────────────────────────────────────────────────

/// How a view should start up.
#[derive(Clone)]
pub struct WebViewConfig {
    /// The page to load first.
    pub source: WebViewSource,
    /// Initial size in physical pixels. Corrected on the first [`WebView::show`].
    pub size: PhysicalSize<u32>,
    /// Navigation and resource-load policy for this view. `None` uses
    /// [`DefaultHandler`]'s behaviour: allow the initial load, deny every
    /// later navigation (reporting it as [`WebViewEvent::LinkClicked`]), and
    /// never intercept resources.
    pub handler: Option<Rc<RefCell<dyn WebViewHandler>>>,
}

impl WebViewConfig {
    /// A config that loads `source` at a default size, with the default
    /// navigation/interception policy.
    pub fn new(source: WebViewSource) -> Self {
        Self {
            source,
            size: PhysicalSize::new(1024, 768),
            handler: None,
        }
    }

    /// Use `handler` for this view's navigation and resource-interception
    /// decisions instead of the default policy.
    pub fn with_handler(mut self, handler: Rc<RefCell<dyn WebViewHandler>>) -> Self {
        self.handler = Some(handler);
        self
    }
}

/// One embedded web view, drawn with [`WebView::show`].
///
/// Created by [`WebViewHost::new_view`]. **Must be driven on the UI thread.**
pub struct WebView {
    servo_view: ServoWebView,
    /// The offscreen GL framebuffer that Servo renders into.
    offscreen_ctx: Rc<OffscreenRenderingContext>,
    events: Rc<RefCell<Vec<WebViewEvent>>>,
    /// Reused across frames; reallocating one per frame was measurable waste.
    texture: Option<egui::TextureHandle>,
    /// Unique per view, so two views cannot collide on one egui texture.
    texture_name: String,
    last_phys_size: PhysicalSize<u32>,
    last_mouse_pos: Option<egui::Pos2>,
}

impl WebView {
    // ── Public helpers ────────────────────────────────────────────────────────

    /// Navigate to a new source programmatically.
    pub fn load(&self, source: WebViewSource) {
        self.servo_view.load(Self::source_to_url(&source));
    }

    /// Reload the current page.
    ///
    /// There is no `stop()`: `servo::WebView` 0.1.0 has no way to cancel an
    /// in-flight load once it has started. The only control points are up
    /// front, via [`WebView::load`] and resource interception
    /// ([`WebViewHandler::intercept`]).
    pub fn reload(&self) {
        self.servo_view.reload();
    }

    /// Whether [`WebView::go_back`] would do anything.
    pub fn can_go_back(&self) -> bool {
        self.servo_view.can_go_back()
    }

    /// Step back `amount` entries in the joint session history.
    pub fn go_back(&self, amount: usize) {
        self.servo_view.go_back(amount);
    }

    /// Whether [`WebView::go_forward`] would do anything.
    pub fn can_go_forward(&self) -> bool {
        self.servo_view.can_go_forward()
    }

    /// Step forward `amount` entries in the joint session history.
    pub fn go_forward(&self, amount: usize) {
        self.servo_view.go_forward(amount);
    }

    /// The view's current URL, if it has navigated anywhere yet.
    pub fn url(&self) -> Option<Url> {
        self.servo_view.url()
    }

    /// The current page's title, if the page has set one.
    pub fn page_title(&self) -> Option<String> {
        self.servo_view.page_title()
    }

    /// The current status text (e.g. a hovered link's target), if any.
    pub fn status_text(&self) -> Option<String> {
        self.servo_view.status_text()
    }

    /// The current page's favicon, if it has one and it has finished loading.
    pub fn favicon(&self) -> Option<Image> {
        self.servo_view.favicon().map(|image| image.clone())
    }

    /// Where the view currently is in its own load lifecycle.
    pub fn load_status(&self) -> LoadStatus {
        self.servo_view.load_status()
    }

    /// Draw the view into `ui` and return any queued [`WebViewEvent`]s.
    ///
    /// Call once per frame per view. The engine itself is driven separately by
    /// [`WebViewHost::spin`], which must be called once per frame overall.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Vec<WebViewEvent> {
        let dpi = ui.ctx().pixels_per_point();

        // Claim the rect first, then size the engine to exactly what we claimed.
        // Deriving the size from `available_size()` while painting into
        // `available_rect_before_wrap()` let the two disagree.
        let resp = ui.allocate_rect(
            ui.available_rect_before_wrap(),
            egui::Sense::click_and_drag(),
        );
        let widget_rect = resp.rect;

        let (phys_w, phys_h) = Self::physical_size(widget_rect.size(), dpi);
        let phys_size = PhysicalSize::new(phys_w, phys_h);

        if phys_size != self.last_phys_size {
            // Resize the view only. `WebView::resize` calls
            // `resize_rendering_context` internally, and resizing the offscreen
            // context ourselves first makes that a no-op (it early-outs on an
            // unchanged size), leaving the page laid out at the old width.
            self.servo_view.resize(phys_size);
            self.servo_view.set_hidpi_scale_factor(
                Scale::<f32, DeviceIndependentPixel, DevicePixel>::new(dpi),
            );
            log::debug!("resize {:?} -> {:?} (dpi {dpi})", self.last_phys_size, phys_size);
            self.last_phys_size = phys_size;
        }

        // Paint Servo's current frame into the offscreen framebuffer, after any
        // resize above so this frame targets the size we are about to read.
        self.servo_view.paint();

        // ── Blit offscreen framebuffer → egui texture ─────────────────────────
        let read_rect = euclid::Box2D::<i32, DevicePixel>::new(
            euclid::Point2D::new(0, 0),
            euclid::Point2D::new(phys_w as i32, phys_h as i32),
        );

        let mut drew = false;
        if let Some(rgba) = self.offscreen_ctx.read_to_image(read_rect) {
            let w = rgba.width() as usize;
            let h = rgba.height() as usize;
            if w > 0 && h > 0 {
                let color_image =
                    egui::ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
                // Reuse one texture for the life of the view. `load_texture`
                // allocates a new one on every call, which meant a fresh
                // full-surface texture every frame.
                let texture = match &mut self.texture {
                    Some(handle) => {
                        handle.set(color_image, egui::TextureOptions::LINEAR);
                        handle
                    }
                    slot => slot.insert(ui.ctx().load_texture(
                        &self.texture_name,
                        color_image,
                        egui::TextureOptions::LINEAR,
                    )),
                };
                ui.painter().image(
                    texture.id(),
                    widget_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
                drew = true;
            }
        }

        if !drew {
            ui.painter()
                .rect_filled(widget_rect, 0.0, egui::Color32::from_gray(20));
        }

        // ── Input forwarding to Servo ─────────────────────────────────────────

        let mut primary_down = false;
        let mut primary_up = false;
        let mut interact_pos = None;
        ui.input(|i| {
            primary_down = i.pointer.button_pressed(egui::PointerButton::Primary);
            primary_up = i.pointer.button_released(egui::PointerButton::Primary);
            interact_pos = i.pointer.interact_pos().or(i.pointer.hover_pos());
        });

        if let Some(pos) = interact_pos {
            // We only send clicks to servo if the mouse is over the webview
            if widget_rect.contains(pos) || resp.dragged() {
                let dp = Self::egui_to_servo_point(pos, widget_rect.min, dpi);
                
                if primary_down {
                    self.servo_view.focus();
                    self.servo_view
                        .notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                            MouseButtonAction::Down,
                            MouseButton::Left,
                            dp,
                        )));
                }
                
                if primary_up {
                    self.servo_view
                        .notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                            MouseButtonAction::Up,
                            MouseButton::Left,
                            dp,
                        )));
                }
            }
        }

        // Mouse move - send AFTER button events so Down is seen before the first drag-move
        if let Some(pos) = interact_pos {
            if widget_rect.contains(pos) || resp.dragged() || primary_up {
                if self.last_mouse_pos != Some(pos) {
                    let dp = Self::egui_to_servo_point(pos, widget_rect.min, dpi);
                    self.servo_view
                        .notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(dp)));
                    self.last_mouse_pos = Some(pos);
                }
            }
        } else {
            self.last_mouse_pos = None;
        }

        // ── Mouse wheel / touchpad scroll ─────────────────────────────────────
        // Use raw_scroll_delta for wheel ticks; smooth_scroll_delta for touchpad.
        // We use smooth_scroll_delta so both mice wheels and touchpads work.
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        if scroll.x != 0.0 || scroll.y != 0.0 {
            let hover = ui
                .input(|i| i.pointer.hover_pos())
                .unwrap_or(widget_rect.center());
            // Scroll even if pointer is slightly outside – common while using wheel
            let center_pt = Self::egui_to_servo_point(widget_rect.center(), widget_rect.min, dpi);
            let scroll_pt = if widget_rect.contains(hover) {
                Self::egui_to_servo_point(hover, widget_rect.min, dpi)
            } else {
                center_pt
            };
            // egui: positive y = content moves up (scroll down).
            // Servo Scroll::Delta: positive y = scroll down (reveal more below).
            // So we negate egui's y to match Servo's convention.
            let vec = WebViewVector::Device(DeviceVector2D::new(
                (-scroll.x * dpi) as f32,
                (-scroll.y * dpi) as f32,
            ));
            self.servo_view
                .notify_scroll_event(Scroll::Delta(vec), scroll_pt);
        }

        // ── Arrow key / Page scrolling ────────────────────────────────────────
        // Only handle keys when the webview is focused (pointer inside or clicked).
        let has_focus = resp.hovered() || resp.clicked() || resp.has_focus();
        if has_focus {
            // Line-height in device pixels for arrow key steps.
            let line_px = (24.0 * dpi) as f32;
            let page_px = (phys_h as f32) * 0.85;
            let center = Self::egui_to_servo_point(widget_rect.center(), widget_rect.min, dpi);

            let keys_pressed = ui.input(|i| i.keys_down.clone());
            for key in &keys_pressed {
                let delta: Option<(f32, f32)> = match key {
                    egui::Key::ArrowDown  => Some((0.0,  line_px)),
                    egui::Key::ArrowUp    => Some((0.0, -line_px)),
                    egui::Key::ArrowRight => Some(( line_px, 0.0)),
                    egui::Key::ArrowLeft  => Some((-line_px, 0.0)),
                    egui::Key::PageDown   => Some((0.0,  page_px)),
                    egui::Key::PageUp     => Some((0.0, -page_px)),
                    egui::Key::Home       => {
                        self.servo_view.notify_scroll_event(Scroll::Start, center);
                        None
                    }
                    egui::Key::End => {
                        self.servo_view.notify_scroll_event(Scroll::End, center);
                        None
                    }
                    _ => None,
                };
                if let Some((dx, dy)) = delta {
                    let vec = WebViewVector::Device(DeviceVector2D::new(dx, dy));
                    self.servo_view.notify_scroll_event(Scroll::Delta(vec), center);
                }
            }

            // Also forward key events so the page can handle them (e.g. form inputs).
            for event in ui.input(|i| i.events.clone()) {
                if let egui::Event::Key { key, pressed, repeat, modifiers, .. } = event {
                    // Skip arrow/page keys – we handle those as scroll above.
                    let is_scroll_key = matches!(
                        key,
                        egui::Key::ArrowUp | egui::Key::ArrowDown |
                        egui::Key::ArrowLeft | egui::Key::ArrowRight |
                        egui::Key::PageUp | egui::Key::PageDown |
                        egui::Key::Home | egui::Key::End
                    );
                    if !is_scroll_key {
                        let state = if pressed { KeyState::Down } else { KeyState::Up };
                        let kb_event = KeyboardEvent::new(keyboard_types::KeyboardEvent {
                            state,
                            key: egui_key_to_keyboard_types(&key),
                            code: egui_key_to_code(&key),
                            location: Location::Standard,
                            modifiers: egui_modifiers_to_keyboard_types(&modifiers),
                            repeat,
                            is_composing: false,
                        });
                        self.servo_view
                            .notify_input_event(InputEvent::Keyboard(kb_event));
                    }
                }
            }
        }

        // Drain accumulated events for the caller.
        std::mem::take(&mut *self.events.borrow_mut())
    }

    // ─── Private helpers ──────────────────────────────────────────────────────

    /// Widget size in logical points -> physical pixels, clamped to at least
    /// 1x1 so a collapsed or zero-sized layout never asks for an empty surface.
    fn physical_size(size: egui::Vec2, dpi: f32) -> (u32, u32) {
        let to_px = |v: f32| {
            let px = v * dpi;
            if px.is_finite() && px >= 1.0 {
                px.round() as u32
            } else {
                1
            }
        };
        (to_px(size.x), to_px(size.y))
    }

    /// Convert an egui logical-pixel position to a Servo device-pixel `WebViewPoint`,
    /// relative to the top-left corner of the webview widget.
    fn egui_to_servo_point(pos: egui::Pos2, origin: egui::Pos2, dpi: f32) -> WebViewPoint {
        use euclid::Point2D;
        let x = (pos.x - origin.x) * dpi;
        let y = (pos.y - origin.y) * dpi;
        WebViewPoint::Device(Point2D::new(x, y))
    }

    /// Convert a [`WebViewSource`] into a [`Url`] Servo can load.
    /// `Html`/`HtmlWithBase` are base64-encoded into a `data:` URL so no
    /// server is needed.
    fn source_to_url(source: &WebViewSource) -> Url {
        match source {
            WebViewSource::Url(u) => Url::parse(u).unwrap_or_else(|_| {
                Url::parse("about:blank").expect("about:blank is always valid")
            }),
            WebViewSource::Html(html) => Self::html_to_data_url(html),
            WebViewSource::HtmlWithBase { html, base } => {
                // A `data:` URL's own address is its effective base, so a
                // relative `href`/`src` in the HTML resolves against
                // "data:...", which is never what the caller wants. Servo has
                // no separate "load this data with that base" entry point, so
                // give the document an explicit base the same way any HTML
                // author would.
                let escaped_base = base.replace('&', "&amp;").replace('"', "&quot;");
                let with_base = format!("<base href=\"{escaped_base}\">{html}");
                Self::html_to_data_url(&with_base)
            }
        }
    }

    fn html_to_data_url(html: &str) -> Url {
        let b64 = general_purpose::STANDARD.encode(html.as_bytes());
        let s = format!("data:text/html;charset=utf-8;base64,{}", b64);
        Url::parse(&s).expect("data URL is always valid")
    }
}

// ─── Key mapping helpers ──────────────────────────────────────────────────────

fn egui_key_to_keyboard_types(key: &egui::Key) -> keyboard_types::Key {
    use keyboard_types::{Key, NamedKey};
    match key {
        egui::Key::Enter       => Key::Named(NamedKey::Enter),
        egui::Key::Tab         => Key::Named(NamedKey::Tab),
        egui::Key::Space       => Key::Character(" ".into()),
        egui::Key::Backspace   => Key::Named(NamedKey::Backspace),
        egui::Key::Delete      => Key::Named(NamedKey::Delete),
        egui::Key::Escape      => Key::Named(NamedKey::Escape),
        egui::Key::F1          => Key::Named(NamedKey::F1),
        egui::Key::F2          => Key::Named(NamedKey::F2),
        egui::Key::F3          => Key::Named(NamedKey::F3),
        egui::Key::F4          => Key::Named(NamedKey::F4),
        egui::Key::F5          => Key::Named(NamedKey::F5),
        egui::Key::F6          => Key::Named(NamedKey::F6),
        egui::Key::F7          => Key::Named(NamedKey::F7),
        egui::Key::F8          => Key::Named(NamedKey::F8),
        egui::Key::F9          => Key::Named(NamedKey::F9),
        egui::Key::F10         => Key::Named(NamedKey::F10),
        egui::Key::F11         => Key::Named(NamedKey::F11),
        egui::Key::F12         => Key::Named(NamedKey::F12),
        egui::Key::ArrowDown   => Key::Named(NamedKey::ArrowDown),
        egui::Key::ArrowUp     => Key::Named(NamedKey::ArrowUp),
        egui::Key::ArrowLeft   => Key::Named(NamedKey::ArrowLeft),
        egui::Key::ArrowRight  => Key::Named(NamedKey::ArrowRight),
        egui::Key::Home        => Key::Named(NamedKey::Home),
        egui::Key::End         => Key::Named(NamedKey::End),
        egui::Key::PageUp      => Key::Named(NamedKey::PageUp),
        egui::Key::PageDown    => Key::Named(NamedKey::PageDown),
        // Letter / digit keys (lowercase; browser handles Shift for uppercase)
        egui::Key::A => Key::Character("a".into()),
        egui::Key::B => Key::Character("b".into()),
        egui::Key::C => Key::Character("c".into()),
        egui::Key::D => Key::Character("d".into()),
        egui::Key::E => Key::Character("e".into()),
        egui::Key::F => Key::Character("f".into()),
        egui::Key::G => Key::Character("g".into()),
        egui::Key::H => Key::Character("h".into()),
        egui::Key::I => Key::Character("i".into()),
        egui::Key::J => Key::Character("j".into()),
        egui::Key::K => Key::Character("k".into()),
        egui::Key::L => Key::Character("l".into()),
        egui::Key::M => Key::Character("m".into()),
        egui::Key::N => Key::Character("n".into()),
        egui::Key::O => Key::Character("o".into()),
        egui::Key::P => Key::Character("p".into()),
        egui::Key::Q => Key::Character("q".into()),
        egui::Key::R => Key::Character("r".into()),
        egui::Key::S => Key::Character("s".into()),
        egui::Key::T => Key::Character("t".into()),
        egui::Key::U => Key::Character("u".into()),
        egui::Key::V => Key::Character("v".into()),
        egui::Key::W => Key::Character("w".into()),
        egui::Key::X => Key::Character("x".into()),
        egui::Key::Y => Key::Character("y".into()),
        egui::Key::Z => Key::Character("z".into()),
        egui::Key::Num0 => Key::Character("0".into()),
        egui::Key::Num1 => Key::Character("1".into()),
        egui::Key::Num2 => Key::Character("2".into()),
        egui::Key::Num3 => Key::Character("3".into()),
        egui::Key::Num4 => Key::Character("4".into()),
        egui::Key::Num5 => Key::Character("5".into()),
        egui::Key::Num6 => Key::Character("6".into()),
        egui::Key::Num7 => Key::Character("7".into()),
        egui::Key::Num8 => Key::Character("8".into()),
        egui::Key::Num9 => Key::Character("9".into()),
        _ => Key::Named(NamedKey::Unidentified),
    }
}

fn egui_key_to_code(key: &egui::Key) -> keyboard_types::Code {
    use keyboard_types::Code;
    match key {
        egui::Key::A => Code::KeyA,
        egui::Key::B => Code::KeyB,
        egui::Key::C => Code::KeyC,
        egui::Key::D => Code::KeyD,
        egui::Key::E => Code::KeyE,
        egui::Key::F => Code::KeyF,
        egui::Key::G => Code::KeyG,
        egui::Key::H => Code::KeyH,
        egui::Key::I => Code::KeyI,
        egui::Key::J => Code::KeyJ,
        egui::Key::K => Code::KeyK,
        egui::Key::L => Code::KeyL,
        egui::Key::M => Code::KeyM,
        egui::Key::N => Code::KeyN,
        egui::Key::O => Code::KeyO,
        egui::Key::P => Code::KeyP,
        egui::Key::Q => Code::KeyQ,
        egui::Key::R => Code::KeyR,
        egui::Key::S => Code::KeyS,
        egui::Key::T => Code::KeyT,
        egui::Key::U => Code::KeyU,
        egui::Key::V => Code::KeyV,
        egui::Key::W => Code::KeyW,
        egui::Key::X => Code::KeyX,
        egui::Key::Y => Code::KeyY,
        egui::Key::Z => Code::KeyZ,
        egui::Key::Num0 => Code::Digit0,
        egui::Key::Num1 => Code::Digit1,
        egui::Key::Num2 => Code::Digit2,
        egui::Key::Num3 => Code::Digit3,
        egui::Key::Num4 => Code::Digit4,
        egui::Key::Num5 => Code::Digit5,
        egui::Key::Num6 => Code::Digit6,
        egui::Key::Num7 => Code::Digit7,
        egui::Key::Num8 => Code::Digit8,
        egui::Key::Num9 => Code::Digit9,
        egui::Key::Enter => Code::Enter,
        egui::Key::Escape => Code::Escape,
        egui::Key::Backspace => Code::Backspace,
        egui::Key::Tab => Code::Tab,
        egui::Key::Space => Code::Space,
        egui::Key::Delete => Code::Delete,
        egui::Key::ArrowDown => Code::ArrowDown,
        egui::Key::ArrowUp => Code::ArrowUp,
        egui::Key::ArrowLeft => Code::ArrowLeft,
        egui::Key::ArrowRight => Code::ArrowRight,
        egui::Key::Home => Code::Home,
        egui::Key::End => Code::End,
        egui::Key::PageUp => Code::PageUp,
        egui::Key::PageDown => Code::PageDown,
        _ => Code::Unidentified,
    }
}

fn egui_modifiers_to_keyboard_types(m: &egui::Modifiers) -> Modifiers {
    let mut out = Modifiers::empty();
    if m.shift { out |= Modifiers::SHIFT; }
    if m.ctrl  { out |= Modifiers::CONTROL; }
    if m.alt   { out |= Modifiers::ALT; }
    if m.mac_cmd { out |= Modifiers::META; }
    // Note: on Windows, command == ctrl, so we don't add META for command.
    // On Mac, command == mac_cmd, so we add META via mac_cmd.
    out
}

// ─── Tests ───────────────────────────────────────────────────────────────────
//
// Only the pure helpers are covered here. Anything touching `Servo`,
// `WebViewHost` or `WebView` construction needs a live GL context and a real
// window, so it cannot run under `cargo test`; that path is exercised instead
// by the esmail binary's ESMAIL_PREVIEW + ESMAIL_SCREENSHOT mode.

#[cfg(test)]
mod tests {
    use super::*;

    // ── physical_size ────────────────────────────────────────────────────────

    #[test]
    fn physical_size_scales_by_dpi_and_rounds() {
        assert_eq!(WebView::physical_size(egui::vec2(100.0, 50.0), 1.0), (100, 50));
        assert_eq!(WebView::physical_size(egui::vec2(100.0, 50.0), 2.0), (200, 100));
        // 1264.0 * 1.25 == 1580.0 exactly; 100.4 * 1.25 == 125.5, which rounds to 126.
        assert_eq!(WebView::physical_size(egui::vec2(1264.0, 100.4), 1.25), (1580, 126));
    }

    #[test]
    fn physical_size_never_returns_zero() {
        // A collapsed panel, a zero-height layout or a hidden tab must never ask
        // Servo for an empty surface.
        assert_eq!(WebView::physical_size(egui::vec2(0.0, 0.0), 1.0), (1, 1));
        assert_eq!(WebView::physical_size(egui::vec2(0.4, 800.0), 1.0), (1, 800));
        assert_eq!(WebView::physical_size(egui::vec2(-10.0, 10.0), 1.0), (1, 10));
    }

    #[test]
    fn physical_size_rejects_non_finite() {
        assert_eq!(WebView::physical_size(egui::vec2(f32::NAN, 10.0), 1.0), (1, 10));
        assert_eq!(WebView::physical_size(egui::vec2(f32::INFINITY, 10.0), 1.0), (1, 10));
        assert_eq!(WebView::physical_size(egui::vec2(10.0, 10.0), f32::NAN), (1, 1));
    }

    // ── source_to_url ────────────────────────────────────────────────────────

    #[test]
    fn url_source_passes_through() {
        let url = WebView::source_to_url(&WebViewSource::Url("https://servo.org/a?b=c".into()));
        assert_eq!(url.as_str(), "https://servo.org/a?b=c");
    }

    #[test]
    fn unparseable_url_falls_back_to_blank_rather_than_panicking() {
        let url = WebView::source_to_url(&WebViewSource::Url("not a url".into()));
        assert_eq!(url.as_str(), "about:blank");
    }

    #[test]
    fn html_source_round_trips_through_a_data_url() {
        // Mail bodies are full of non-ASCII; it must survive the base64 hop.
        let html = "<p>café € &amp; \"quotes\"</p>";
        let url = WebView::source_to_url(&WebViewSource::Html(html.to_string()));

        let encoded = url
            .as_str()
            .strip_prefix("data:text/html;charset=utf-8;base64,")
            .expect("should be a base64 data URL");
        let decoded = general_purpose::STANDARD
            .decode(encoded)
            .expect("should be valid base64");

        assert_eq!(String::from_utf8(decoded).unwrap(), html);
    }

    #[test]
    fn empty_html_is_still_a_valid_url() {
        let url = WebView::source_to_url(&WebViewSource::Html(String::new()));
        assert_eq!(url.as_str(), "data:text/html;charset=utf-8;base64,");
    }

    #[test]
    fn html_with_base_injects_a_base_tag_so_relative_links_resolve() {
        // A `data:` URL's own address is its base, so a plain `Html` source
        // can never resolve a relative link/resource. `HtmlWithBase` fixes
        // that by giving the document an explicit `<base href>`.
        let url = WebView::source_to_url(&WebViewSource::HtmlWithBase {
            html: "<p><a href=\"reply\">reply</a></p>".to_string(),
            base: "https://mail.example.com/inbox/42/".to_string(),
        });

        let encoded = url
            .as_str()
            .strip_prefix("data:text/html;charset=utf-8;base64,")
            .expect("should be a base64 data URL");
        let decoded = general_purpose::STANDARD.decode(encoded).unwrap();
        let html = String::from_utf8(decoded).unwrap();

        assert_eq!(
            html,
            "<base href=\"https://mail.example.com/inbox/42/\"><p><a href=\"reply\">reply</a></p>"
        );
    }

    #[test]
    fn html_with_base_escapes_quotes_and_ampersands_in_the_base() {
        // The base is spliced into an HTML attribute; an unescaped `"` in it
        // would let the base URL close the attribute early and inject markup.
        let url = WebView::source_to_url(&WebViewSource::HtmlWithBase {
            html: "<p>hi</p>".to_string(),
            base: "https://example.com/\"><script>evil()</script>&x=1".to_string(),
        });

        let encoded = url
            .as_str()
            .strip_prefix("data:text/html;charset=utf-8;base64,")
            .unwrap();
        let html = String::from_utf8(general_purpose::STANDARD.decode(encoded).unwrap()).unwrap();

        // The base's own `"` and `&` must come through as entities, so the
        // whole thing stays inert text inside the attribute value rather than
        // closing it early and turning `<script>` into a real element.
        assert_eq!(
            html,
            "<base href=\"https://example.com/&quot;><script>evil()</script>&amp;x=1\"><p>hi</p>"
        );
    }

    // ── navigation & interception policy ─────────────────────────────────────

    #[test]
    fn default_handler_denies_navigation_and_never_intercepts() {
        let mut handler = DefaultHandler;
        let url = Url::parse("https://example.com/clicked").unwrap();
        assert_eq!(handler.navigation(&url), NavigationPolicy::Deny);

        let request = WebResourceRequest {
            method: http::Method::GET,
            headers: http::HeaderMap::new(),
            url,
            is_for_main_frame: false,
            is_redirect: false,
        };
        assert!(matches!(handler.intercept(&request), InterceptOutcome::Allow));
    }

    // ── coordinate transform ─────────────────────────────────────────────────

    fn device_xy(p: WebViewPoint) -> (f32, f32) {
        match p {
            WebViewPoint::Device(p) => (p.x, p.y),
            other => panic!("expected a device-space point, got {other:?}"),
        }
    }

    #[test]
    fn point_is_relative_to_the_widget_origin() {
        // A click on the widget's top-left corner is (0, 0) to the page,
        // wherever the widget happens to sit on screen.
        let at_origin =
            WebView::egui_to_servo_point(egui::pos2(300.0, 80.0), egui::pos2(300.0, 80.0), 1.0);
        assert_eq!(device_xy(at_origin), (0.0, 0.0));

        let inside =
            WebView::egui_to_servo_point(egui::pos2(310.0, 100.0), egui::pos2(300.0, 80.0), 1.0);
        assert_eq!(device_xy(inside), (10.0, 20.0));
    }

    #[test]
    fn point_scales_by_dpi() {
        let p = WebView::egui_to_servo_point(egui::pos2(110.0, 90.0), egui::pos2(100.0, 80.0), 2.0);
        assert_eq!(device_xy(p), (20.0, 20.0));
    }

    // ── key and modifier mapping ─────────────────────────────────────────────

    #[test]
    fn named_keys_map_across() {
        use keyboard_types::{Key, NamedKey};
        assert_eq!(egui_key_to_keyboard_types(&egui::Key::Enter), Key::Named(NamedKey::Enter));
        assert_eq!(egui_key_to_keyboard_types(&egui::Key::Escape), Key::Named(NamedKey::Escape));
        assert_eq!(
            egui_key_to_keyboard_types(&egui::Key::ArrowDown),
            Key::Named(NamedKey::ArrowDown)
        );
    }

    #[test]
    fn unmapped_keys_are_unidentified_rather_than_a_panic() {
        use keyboard_types::{Key, NamedKey};
        assert_eq!(
            egui_key_to_keyboard_types(&egui::Key::Insert),
            Key::Named(NamedKey::Unidentified)
        );
    }

    /// Documents a known defect rather than asserting desirable behaviour.
    /// `egui::Key` carries no case information, so this mapping can only ever
    /// produce lowercase and can never produce a shifted symbol — typing "A" or
    /// "@" into a page is impossible. A5 replaces it with `egui::Event::Text`;
    /// delete this test when that lands.
    #[test]
    fn letter_keys_are_lowercase_only_which_a5_must_fix() {
        use keyboard_types::Key;
        assert_eq!(egui_key_to_keyboard_types(&egui::Key::A), Key::Character("a".into()));
    }

    #[test]
    fn modifiers_map_across() {
        let none = egui_modifiers_to_keyboard_types(&egui::Modifiers::default());
        assert!(none.is_empty());

        let shift_ctrl = egui_modifiers_to_keyboard_types(&egui::Modifiers {
            shift: true,
            ctrl: true,
            ..Default::default()
        });
        assert!(shift_ctrl.contains(Modifiers::SHIFT));
        assert!(shift_ctrl.contains(Modifiers::CONTROL));
        assert!(!shift_ctrl.contains(Modifiers::ALT));
    }
}
