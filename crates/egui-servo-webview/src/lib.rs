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

// A7 (PLAN.md): `warn`, not `deny` -- this crate is internal-only (never
// published, see PLAN.md's A7 section), so there's no external consumer to
// protect with a hard failure. `warn` still gets every public item reviewed
// once and keeps new ones from silently going undocumented.
#![warn(missing_docs)]

// Re-exported so callers can name the types in this crate's signatures without
// taking their own dependency on these crates (and risking a version skew).
pub use dpi;
pub use url;
pub use servo::{LoadStatus, WebResourceRequest, Image};

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose};
use dpi::PhysicalSize;
use euclid::Scale;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use url::Url;

use servo::{
    Cursor as ServoCursor, DevicePixel, DeviceVector2D, InputEvent, JSValue,
    OffscreenRenderingContext, RenderingContext, Scroll, Servo, ServoBuilder, WebViewBuilder,
    WebViewDelegate, WebViewPoint, WebViewVector, WindowRenderingContext, NavigationRequest,
    WebResourceLoad, WebResourceResponse,
};
// `servo::WebView` is the engine-side view. Ours (below) wraps it, so alias the
// engine type to keep the two unambiguous at every use site.
use servo::WebView as ServoWebView;
use servo::input_events::{
    EditingActionEvent, ImeEvent as ServoImeEvent, KeyboardEvent, MouseButton, MouseButtonAction,
    MouseButtonEvent, MouseMoveEvent, WheelDelta, WheelEvent, WheelMode,
};
use servo::DeviceIndependentPixel;
// keyboard_types is re-exported by servo. We import it separately to
// construct KeyboardEvent values – use fully-qualified paths to avoid
// conflicts with the servo::Key re-export.
use keyboard_types::{CompositionEvent, CompositionState, KeyState, Location, Modifiers};


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

// ─── Overlay scrollbar (issue #15) ──────────────────────────────────────────
//
// Servo 0.1.0 exposes no scroll-position/content-height getter and no
// absolute "scroll to Y" setter on `servo::WebView` (`notify_scroll_event` is
// write-only and relative), and the engine paints no scrollbar of its own
// into the framebuffer at all -- confirmed against the vendored source and
// against servoshell, Servo's own reference embedder, which also has none.
// See the investigation posted on the GitHub issue for the full writeup.
//
// The only available lever is `WebView::evaluate_javascript`, a real,
// already-exposed, callback-based API that was simply unused until now. This
// section polls `document.scrollingElement`'s three numbers through it and
// paints an ordinary egui track+thumb from the result -- no Servo/vendored
// patching. Two caveats worth keeping in mind (not solved here, just not
// surprising): this is polling, not push-driven, so there is a frame or two
// of lag right after a wheel-scroll; and `document.scrollingElement` will not
// track a page that overrides the default scrolling element (a nested
// `overflow:auto` container) -- fine for typical mail bodies, not general.

/// The three numbers the overlay scrollbar is drawn from, polled off
/// `document.scrollingElement` via `evaluate_javascript`. All in CSS pixels,
/// as the DOM reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ScrollInfo {
    scroll_top: f64,
    scroll_height: f64,
    client_height: f64,
}

/// The script polled each `show_impl` tick (throttled by
/// [`WebView::SCROLL_POLL_INTERVAL`]). `document.scrollingElement` is null
/// only in edge cases (e.g. no `<body>` yet), hence the `documentElement`
/// fallback -- either way this always returns a 3-element array so
/// [`js_value_to_scroll_info`] has one shape to parse.
const SCROLL_INFO_SCRIPT: &str = "(() => { \
    const el = document.scrollingElement || document.documentElement; \
    if (!el) { return [0, 0, 0]; } \
    return [el.scrollTop, el.scrollHeight, el.clientHeight]; \
})()";

/// Parse the `[scrollTop, scrollHeight, clientHeight]` array
/// [`SCROLL_INFO_SCRIPT`] returns out of the raw [`JSValue`]
/// `evaluate_javascript`'s callback hands back. `None` for any shape other
/// than the exact one expected -- a page-side error or a future engine change
/// should leave the last-known scrollbar state alone, not panic or draw
/// garbage.
fn js_value_to_scroll_info(value: &JSValue) -> Option<ScrollInfo> {
    let JSValue::Array(items) = value else {
        return None;
    };
    let [top, height, client] = <[JSValue; 3]>::try_from(items.clone()).ok()?;
    let as_f64 = |v: &JSValue| match v {
        JSValue::Number(n) => Some(*n),
        _ => None,
    };
    Some(ScrollInfo {
        scroll_top: as_f64(&top)?,
        scroll_height: as_f64(&height)?,
        client_height: as_f64(&client)?,
    })
}

// ─── Internal delegate ───────────────────────────────────────────────────────

struct Delegate {
    egui_ctx: egui::Context,
    /// Events queued during delegate callbacks; drained by `show()` each frame.
    events: Rc<RefCell<Vec<WebViewEvent>>>,
    /// Track whether the very first load has been dispatched so we can
    /// distinguish the initial navigation from user-initiated link clicks.
    initial_load_done: Rc<RefCell<bool>>,
    handler: Rc<RefCell<dyn WebViewHandler>>,
    /// Set whenever Servo has painted a new frame; cleared by `show()` once
    /// it has re-read the framebuffer. The `read_to_image` round-trip is not
    /// cheap, so re-reading only when a new frame actually landed (rather
    /// than on every `show()`) is worth doing (A6, PLAN.md).
    frame_dirty: Rc<Cell<bool>>,
    /// The page's last-requested cursor, mapped to `egui::CursorIcon`. Shared
    /// with `WebView`, which applies it via `ctx.set_cursor_icon` on every
    /// `show()` while the pointer is over the widget (A5, PLAN.md).
    cursor: Rc<Cell<egui::CursorIcon>>,
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _webview: ServoWebView) {
        self.frame_dirty.set(true);
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

    fn notify_cursor_changed(&self, _webview: ServoWebView, cursor: ServoCursor) {
        self.cursor.set(servo_cursor_to_egui_cursor_icon(cursor));
        // Nothing else re-reads `cursor` off its own bat -- request a repaint
        // so `show()` runs again and applies it via `ctx.set_cursor_icon`
        // this frame rather than waiting for some unrelated repaint.
        self.egui_ctx.request_repaint();
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
        // Start dirty so the CPU fallback path's first `show()` reads a frame
        // even if `notify_new_frame_ready` hasn't fired yet.
        let frame_dirty: Rc<Cell<bool>> = Rc::new(Cell::new(true));
        let cursor: Rc<Cell<egui::CursorIcon>> = Rc::new(Cell::new(egui::CursorIcon::Default));
        let handler = config
            .handler
            .clone()
            .unwrap_or_else(|| Rc::new(RefCell::new(DefaultHandler)));

        let delegate = Rc::new(Delegate {
            egui_ctx: egui_ctx.clone(),
            events: events.clone(),
            initial_load_done,
            handler,
            frame_dirty: frame_dirty.clone(),
            cursor: cursor.clone(),
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
            frame_dirty,
            cursor,
            texture: None,
            texture_name: format!("egui_servo_webview_{view_id}"),
            last_phys_size: config.size,
            last_mouse_pos: None,
            scroll_info: Rc::new(Cell::new(ScrollInfo::default())),
            last_scroll_poll: None,
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
    /// Shared with the [`Delegate`]; see its field doc.
    frame_dirty: Rc<Cell<bool>>,
    /// Shared with the [`Delegate`]; see its field doc.
    cursor: Rc<Cell<egui::CursorIcon>>,
    /// Reused across frames; reallocating one per frame was measurable waste.
    texture: Option<egui::TextureHandle>,
    /// Unique per view, so two views cannot collide on one egui texture.
    texture_name: String,
    last_phys_size: PhysicalSize<u32>,
    last_mouse_pos: Option<egui::Pos2>,
    /// Last-polled `document.scrollingElement` numbers. Shared (`Rc<Cell<_>>`,
    /// the same pattern `frame_dirty`/`cursor` use) because
    /// [`WebView::poll_scroll_info`]'s `evaluate_javascript` callback writes
    /// into it asynchronously, from outside `show_impl`'s `&mut self`. See
    /// the "Overlay scrollbar" section above.
    scroll_info: Rc<Cell<ScrollInfo>>,
    /// When [`WebView::poll_scroll_info`] last fired, so it can be throttled
    /// to [`WebView::SCROLL_POLL_INTERVAL`] instead of once per `show_impl`.
    last_scroll_poll: Option<Instant>,
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
        self.show_impl(ui).1
    }

    /// Shared implementation behind [`WebView::show`] and the
    /// `egui::Widget for &mut WebView` impl below -- the two differ only in
    /// which half of this they hand back to the caller.
    fn show_impl(&mut self, ui: &mut egui::Ui) -> (egui::Response, Vec<WebViewEvent>) {
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
        //
        // A6 (PLAN.md) attempted the zero-copy `render_to_parent_callback` +
        // `egui::PaintCallback` path here and it does not work in this crate's
        // architecture -- see PLAN.md's A6 section for the full account of why
        // (screenshot came back a uniform blank fill; root-caused to
        // `WindowRenderingContext::new` building Servo its own independent GL
        // context on the window handle, never shared with eframe/glutin's own
        // context, so the "parent" framebuffer `render_to_parent_callback`
        // blits into is not the framebuffer `egui_glow` is actually
        // compositing into). Reverted in favor of the CPU readback path,
        // which is what actually renders correctly. The other half of A6 did
        // land: only re-read when a new frame has arrived since the last one
        // (`frame_dirty`, set by `notify_new_frame_ready`), rather than
        // unconditionally on every `show()`.
        let read_rect = euclid::Box2D::<i32, DevicePixel>::new(
            euclid::Point2D::new(0, 0),
            euclid::Point2D::new(phys_w as i32, phys_h as i32),
        );

        if self.frame_dirty.get() {
            if let Some(rgba) = self.offscreen_ctx.read_to_image(read_rect) {
                let w = rgba.width() as usize;
                let h = rgba.height() as usize;
                if w > 0 && h > 0 {
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
                    // Reuse one texture for the life of the view. `load_texture`
                    // allocates a new one on every call, which meant a fresh
                    // full-surface texture every frame.
                    match &mut self.texture {
                        Some(handle) => handle.set(color_image, egui::TextureOptions::LINEAR),
                        slot => {
                            let _ = slot.insert(ui.ctx().load_texture(
                                &self.texture_name,
                                color_image,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                    }
                }
            }
            self.frame_dirty.set(false);
        }

        // Paint whatever the texture currently holds -- the frame just read
        // above, or (when not dirty) the one from a previous frame, so the
        // widget doesn't flash blank on a `show()` with nothing new.
        let mut drew = false;
        if let Some(texture) = &self.texture {
            ui.painter().image(
                texture.id(),
                widget_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            drew = true;
        }

        if !drew {
            ui.painter()
                .rect_filled(widget_rect, 0.0, egui::Color32::from_gray(20));
        }

        // ── Overlay scrollbar (issue #15) ─────────────────────────────────────
        // See the "Overlay scrollbar" doc comment near `ScrollInfo` above for
        // why this exists and why it's a JS-bridge poll rather than a native
        // Servo getter/setter.
        self.poll_scroll_info();

        let scroll_info = self.scroll_info.get();
        let track_rect = egui::Rect::from_min_max(
            egui::pos2(
                widget_rect.right() - Self::SCROLLBAR_WIDTH - Self::SCROLLBAR_MARGIN,
                widget_rect.top() + Self::SCROLLBAR_MARGIN,
            ),
            egui::pos2(
                widget_rect.right() - Self::SCROLLBAR_MARGIN,
                widget_rect.bottom() - Self::SCROLLBAR_MARGIN,
            ),
        );
        let thumb_metrics = Self::scrollbar_thumb_metrics(
            scroll_info.scroll_top,
            scroll_info.scroll_height,
            scroll_info.client_height,
            track_rect.height(),
        );
        let mut scrollbar_dragging = false;

        if let Some((thumb_top, thumb_height)) = thumb_metrics {
            let thumb_rect = egui::Rect::from_min_size(
                egui::pos2(track_rect.left(), track_rect.top() + thumb_top),
                egui::vec2(track_rect.width(), thumb_height),
            );
            // Hit-test the whole track, not just the thumb -- a thin strip is
            // an easy target to miss, and there is no separate "click track
            // to jump" behaviour to conflict with here.
            let scrollbar_resp =
                ui.interact(track_rect, resp.id.with("scrollbar_thumb"), egui::Sense::drag());
            let scrollbar_hovered = scrollbar_resp.hovered();
            scrollbar_dragging = scrollbar_resp.dragged();

            if scrollbar_dragging {
                if let Some(pointer) = scrollbar_resp.interact_pointer_pos() {
                    let new_scroll_top = Self::scroll_top_for_thumb_center(
                        pointer.y,
                        track_rect.top(),
                        track_rect.height(),
                        thumb_height,
                        scroll_info.scroll_height,
                        scroll_info.client_height,
                    );
                    // Optimistic local update so the thumb tracks the pointer
                    // immediately -- waiting for `poll_scroll_info`'s own
                    // round trip would lag noticeably mid-drag.
                    self.scroll_info.set(ScrollInfo { scroll_top: new_scroll_top, ..scroll_info });
                    // Fire-and-forget, matching `evaluate_javascript`'s async
                    // style elsewhere. `window.scrollTo` is the only "jump to
                    // an absolute position" lever available -- Servo's own
                    // `notify_scroll_event` is relative-delta only.
                    self.servo_view
                        .evaluate_javascript(format!("window.scrollTo(0, {new_scroll_top})"), |_| {});
                }
            }

            let alpha = if scrollbar_dragging {
                160
            } else if scrollbar_hovered {
                130
            } else {
                90
            };
            ui.painter().rect_filled(
                track_rect,
                Self::SCROLLBAR_WIDTH / 2.0,
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 18),
            );
            ui.painter().rect_filled(
                thumb_rect,
                Self::SCROLLBAR_WIDTH / 2.0,
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, alpha),
            );
        }

        // A pointer over the scrollbar track (dragging it or not) must not
        // also forward clicks/moves to the page underneath -- without this a
        // click on the thumb both drags it and lands on whatever page
        // element happens to be there.
        let over_scrollbar = |pos: egui::Pos2| thumb_metrics.is_some() && track_rect.contains(pos);

        // ── Input forwarding to Servo ─────────────────────────────────────────
        // Buttons other than Primary added per A5 (PLAN.md) — nothing upstream
        // needed to teach us those, servoshell just forwards all of winit's.

        let mut buttons_down: Vec<(egui::PointerButton, MouseButton)> = Vec::new();
        let mut buttons_up: Vec<(egui::PointerButton, MouseButton)> = Vec::new();
        let mut interact_pos = None;
        ui.input(|i| {
            for (egui_button, servo_button) in [
                (egui::PointerButton::Primary, MouseButton::Left),
                (egui::PointerButton::Secondary, MouseButton::Right),
                (egui::PointerButton::Middle, MouseButton::Middle),
            ] {
                if i.pointer.button_pressed(egui_button) {
                    buttons_down.push((egui_button, servo_button));
                }
                if i.pointer.button_released(egui_button) {
                    buttons_up.push((egui_button, servo_button));
                }
            }
            interact_pos = i.pointer.interact_pos().or(i.pointer.hover_pos());
        });
        let primary_up = buttons_up.iter().any(|(b, _)| *b == egui::PointerButton::Primary);

        if let Some(pos) = interact_pos {
            // We only send clicks to servo if the mouse is over the webview,
            // and not while it's over/dragging the overlay scrollbar.
            if (widget_rect.contains(pos) || resp.dragged())
                && !scrollbar_dragging
                && !over_scrollbar(pos)
            {
                let dp = Self::egui_to_servo_point(pos, widget_rect.min, dpi);

                if !buttons_down.is_empty() {
                    // A5: focus follows any button, not just a completed
                    // click — request_focus() here (rather than relying on
                    // hover, the pre-A5 approximation) is also what lets the
                    // `resp.has_focus()` gate below actually turn on.
                    self.servo_view.focus();
                    resp.request_focus();
                }
                for (_, servo_button) in &buttons_down {
                    // Diagnostic for issue #21 (text selection/copy not
                    // working): confirms whether a mousedown that should
                    // start a drag-select actually reaches Servo at all, and
                    // at what device-pixel coordinate. `log::debug!` so it's
                    // opt-in via `RUST_LOG=egui_servo_webview=debug` rather
                    // than on by default (`main.rs::init_logging`'s default
                    // filter is `warn`).
                    log::debug!("forwarding MouseButton::Down({servo_button:?}) at {dp:?}");
                    self.servo_view
                        .notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                            MouseButtonAction::Down,
                            *servo_button,
                            dp,
                        )));
                }
                for (_, servo_button) in &buttons_up {
                    log::debug!("forwarding MouseButton::Up({servo_button:?}) at {dp:?}");
                    self.servo_view
                        .notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                            MouseButtonAction::Up,
                            *servo_button,
                            dp,
                        )));
                }
            } else if !buttons_down.is_empty() || !buttons_up.is_empty() {
                // A button transition happened but the guard above
                // suppressed it (outside the widget and not dragging, over
                // the scrollbar, or the scrollbar itself is being dragged) —
                // logged because "nothing happened" and "the event was
                // silently dropped here" look identical from the outside,
                // and issue #21's investigation specifically flagged this
                // guard as a place a real drag could get lost.
                log::debug!(
                    "suppressed MouseButton event(s) at {pos:?}: in_bounds={} dragged={} scrollbar_dragging={} over_scrollbar={}",
                    widget_rect.contains(pos), resp.dragged(), scrollbar_dragging, over_scrollbar(pos)
                );
            }
        }

        // Mouse move - send AFTER button events so Down is seen before the first drag-move
        if let Some(pos) = interact_pos {
            if (widget_rect.contains(pos) || resp.dragged() || primary_up)
                && !scrollbar_dragging
                && !over_scrollbar(pos)
            {
                if self.last_mouse_pos != Some(pos) {
                    let dp = Self::egui_to_servo_point(pos, widget_rect.min, dpi);
                    // `trace!`, not `debug!` -- this fires on every changed
                    // position while dragging, which would otherwise flood
                    // the log. Enable with `RUST_LOG=egui_servo_webview=trace`
                    // to see the full move sequence for a drag-select attempt
                    // (issue #21) alongside the Down/Up `debug!` lines above.
                    log::trace!("forwarding MouseMove to {dp:?}");
                    self.servo_view
                        .notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(dp)));
                    self.last_mouse_pos = Some(pos);
                }
            } else if resp.dragged() {
                log::debug!(
                    "suppressed MouseMove during a drag at {pos:?}: scrollbar_dragging={scrollbar_dragging} over_scrollbar={}",
                    over_scrollbar(pos)
                );
            }
        } else {
            // A5: tell the page the pointer left, so :hover state doesn't
            // stay stuck the way it did when we merely stopped sending moves.
            if self.last_mouse_pos.is_some() {
                self.servo_view
                    .notify_input_event(InputEvent::MouseLeftViewport(Default::default()));
            }
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
            // A5 (PLAN.md): migrated from `notify_scroll_event(Scroll::Delta(..))`
            // (the touch-pan path servoshell only uses on mobile) to
            // `InputEvent::Wheel`, which is what desktop servoshell sends and
            // is the only path that dispatches a DOM `wheel` event a page can
            // `preventDefault()`. `scroll_to_wheel_delta`'s doc comment (and
            // the unit test pinning it below) is the sign-convention research
            // this migration was previously held back for.
            let delta = Self::scroll_to_wheel_delta(scroll, dpi);
            self.servo_view
                .notify_input_event(InputEvent::Wheel(WheelEvent::new(delta, scroll_pt)));
        }

        // ── Arrow key / Page scrolling ────────────────────────────────────────
        // A5: gated on real egui focus now, requested above on click, rather
        // than on hover — hover alone meant typing into a sibling text field
        // with the pointer merely resting over this widget leaked keystrokes
        // into the page underneath it.
        let has_focus = resp.has_focus();
        // Diagnostic for issue #21: Ctrl/Cmd+C is only ever evaluated inside
        // this `if has_focus` block, so if a drag-select never gives the
        // widget egui focus (e.g. the click that started the drag landed
        // outside `widget_rect`, or something else grabbed focus first),
        // Copy is silently never even considered -- this line's absence in
        // the log during a select-then-Ctrl+C attempt is itself the
        // diagnostic. `trace!`, not `debug!`, since it fires every frame the
        // widget lacks focus, which is routine and not itself interesting.
        if !has_focus {
            log::trace!("webview does not have egui focus this frame -- keyboard/EditingAction forwarding skipped");
        }
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

                    // A5 (PLAN.md): Ctrl/Cmd+X/C/V -> `InputEvent::EditingAction`,
                    // mirroring servoshell. The OS-clipboard plumbing itself
                    // (`ClipboardDelegate`) is already there for free on this
                    // platform -- `servo`'s `clipboard` feature is on by
                    // default and installs a real `arboard`-backed delegate
                    // whenever the embedder doesn't supply its own (see
                    // `clipboard_delegate.rs`'s doc comment) -- what was
                    // actually missing is telling Servo *when* to invoke it,
                    // since a keydown alone doesn't imply "run the copy/cut/
                    // paste editing command" the way a browser's own
                    // accelerator table does.
                    if pressed {
                        let shortcut_mod = modifiers.ctrl || modifiers.mac_cmd;
                        let action = if shortcut_mod && !modifiers.shift && !modifiers.alt {
                            match key {
                                egui::Key::C => Some(EditingActionEvent::Copy),
                                egui::Key::X => Some(EditingActionEvent::Cut),
                                egui::Key::V => Some(EditingActionEvent::Paste),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        if let Some(action) = action {
                            // Diagnostic for issue #21: confirms the
                            // Ctrl/Cmd+C/X/V shortcut was actually seen and
                            // forwarded as an EditingAction. If Copy never
                            // does anything, this line appearing (or not)
                            // tells you whether the problem is "the
                            // shortcut never reached this code" (missing
                            // log -- check `has_focus`/the widget not
                            // having keyboard focus, logged below) vs. "it
                            // was sent, but there was nothing selected to
                            // copy, or Servo's own clipboard handling
                            // didn't act on it" (log present, still no
                            // clipboard content).
                            log::debug!("forwarding EditingAction::{action:?}");
                            self.servo_view
                                .notify_input_event(InputEvent::EditingAction(action));
                        }
                    }
                } else if let egui::Event::Text(text) = event {
                    // A5: the actual character(s), shift/layout already
                    // resolved by egui-winit from the same source winit
                    // itself uses — see `text_to_keyboard_events`'s doc
                    // comment for why `egui_key_to_keyboard_types` above no
                    // longer tries to guess this from `egui::Key` alone.
                    let modifiers = ui.input(|i| egui_modifiers_to_keyboard_types(&i.modifiers));
                    for kb_event in text_to_keyboard_events(&text, modifiers) {
                        self.servo_view
                            .notify_input_event(InputEvent::Keyboard(kb_event));
                    }
                } else if let egui::Event::Ime(ime) = event {
                    // A5 (PLAN.md): forward IME composition, mirroring
                    // servoshell's Start/Update/End/Dismissed states. Without
                    // this, dead keys and CJK input are impossible — only
                    // whatever a composition eventually `Commit`s would ever
                    // have reached the page, and only by accident (as a
                    // `Text` event with no composing context).
                    self.servo_view
                        .notify_input_event(InputEvent::Ime(egui_ime_to_servo_ime(ime)));
                }
            }
        }

        // ── Cursor ───────────────────────────────────────────────────────────
        // A5 (PLAN.md): reflect the page's last-requested cursor
        // (`notify_cursor_changed`, via the shared `cursor` cell) while the
        // pointer is actually over this widget. `set_cursor_icon` must be
        // called every frame to "win" -- egui resets to `Default` each frame
        // otherwise -- so this runs unconditionally on every `show()`, not
        // just when the cursor last changed.
        if resp.hovered() {
            ui.ctx().set_cursor_icon(self.cursor.get());
        }

        // Drain accumulated events for the caller.
        let events = std::mem::take(&mut *self.events.borrow_mut());
        (resp, events)
    }

    // ─── Private helpers ──────────────────────────────────────────────────────

    /// Width of the overlay scrollbar's track, in logical points.
    const SCROLLBAR_WIDTH: f32 = 10.0;
    /// Gap between the scrollbar and the widget's edges, in logical points.
    const SCROLLBAR_MARGIN: f32 = 2.0;
    /// Thumb never shrinks below this, in logical points, so a very long page
    /// doesn't collapse it to an unclickable sliver.
    const MIN_THUMB_HEIGHT: f32 = 24.0;
    /// How often [`WebView::poll_scroll_info`] actually issues a new
    /// `evaluate_javascript` call. `show_impl` runs far more often than this
    /// (every repaint), and a scroll position doesn't need sub-frame
    /// freshness -- see the "Overlay scrollbar" section's note on polling lag.
    const SCROLL_POLL_INTERVAL: Duration = Duration::from_millis(150);

    /// Kick off (throttled) an async `evaluate_javascript` re-read of
    /// `document.scrollingElement`'s scroll position/content height, writing
    /// the result into `self.scroll_info` once Servo calls back. Fire-and-
    /// forget: a dropped/errored evaluation just leaves the last-known value
    /// in place for this frame's paint, same as a slow one.
    fn poll_scroll_info(&mut self) {
        let now = Instant::now();
        let due = match self.last_scroll_poll {
            None => true,
            Some(last) => now.duration_since(last) >= Self::SCROLL_POLL_INTERVAL,
        };
        if !due {
            return;
        }
        self.last_scroll_poll = Some(now);

        let scroll_info = self.scroll_info.clone();
        self.servo_view.evaluate_javascript(SCROLL_INFO_SCRIPT, move |result| {
            if let Ok(value) = result {
                if let Some(info) = js_value_to_scroll_info(&value) {
                    scroll_info.set(info);
                }
            }
        });
    }

    /// Compute the thumb's offset from the track's top and its height, both
    /// in the track's own logical-point coordinate space. `None` when the
    /// page doesn't scroll (nothing to show a scrollbar for) or the reported
    /// content height is degenerate.
    ///
    /// Kept pure and free of any egui/Servo types so it can be unit tested
    /// directly against hand-picked scroll numbers, the same way
    /// `physical_size`/`egui_to_servo_point` are above.
    fn scrollbar_thumb_metrics(
        scroll_top: f64,
        scroll_height: f64,
        client_height: f64,
        track_height: f32,
    ) -> Option<(f32, f32)> {
        // A one-pixel slop: `scroll_height` and `client_height` are rarely
        // exactly equal even on a non-scrolling page (subpixel layout), so a
        // strict `>` would flicker a scrollbar in and out for a static page.
        if client_height <= 0.0 || scroll_height <= client_height + 1.0 || track_height <= 0.0 {
            return None;
        }

        let raw_thumb_height = (client_height / scroll_height) as f32 * track_height;
        let thumb_height = raw_thumb_height.clamp(Self::MIN_THUMB_HEIGHT.min(track_height), track_height);

        let max_scroll = scroll_height - client_height;
        let thumb_travel = (track_height - thumb_height).max(0.0);
        let fraction = if max_scroll > 0.0 {
            (scroll_top / max_scroll).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_top = fraction as f32 * thumb_travel;

        Some((thumb_top, thumb_height))
    }

    /// Invert [`WebView::scrollbar_thumb_metrics`]: given where the pointer
    /// is while dragging the thumb, compute the `scrollTop` to send the page
    /// to via `window.scrollTo`. `pointer_y` and `track_top`/`track_height`
    /// are all in the same (widget-local, logical-point) space `show_impl`
    /// already works in.
    fn scroll_top_for_thumb_center(
        pointer_y: f32,
        track_top: f32,
        track_height: f32,
        thumb_height: f32,
        scroll_height: f64,
        client_height: f64,
    ) -> f64 {
        let max_scroll = (scroll_height - client_height).max(0.0);
        let thumb_travel = (track_height - thumb_height).max(1.0);
        // Treat `pointer_y` as where the thumb's *center* should end up,
        // which is what makes dragging feel like it's grabbing the thumb
        // rather than snapping its top edge to the cursor.
        let desired_thumb_top = pointer_y - track_top - thumb_height / 2.0;
        let fraction = (desired_thumb_top / thumb_travel).clamp(0.0, 1.0);
        fraction as f64 * max_scroll
    }

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

    /// Convert egui's `smooth_scroll_delta` (logical points) into a Servo
    /// `WheelDelta` (device pixels), **without negating either axis**.
    ///
    /// This is the sign-convention research A5 (PLAN.md) was held back on.
    /// Two doc comments settle it, both read from the vendored source under
    /// `servo-embedder-traits-0.1.0/input_events.rs`:
    ///
    /// - `WheelDelta::y`: "A positive value means that the view scrolls up,
    ///   revealing more content above the current viewport." (symmetric
    ///   wording for `x`, left/right.)
    /// - Compare `Scroll::Delta`, which this widget used before this
    ///   migration: `servo-paint-0.1.0/webview_renderer.rs`'s
    ///   `notify_input_event_handled` is where Servo itself turns a *received*
    ///   `Wheel` event into the internal `Scroll::Delta` that actually moves
    ///   the page — `let scroll_delta = -wheel_event.delta;` (its own comment:
    ///   "A scroll delta for a wheel event is the inverse of the wheel
    ///   delta."). So `WheelDelta` and `Scroll::Delta` are deliberately
    ///   opposite-signed; this function targets `WheelDelta`, not
    ///   `Scroll::Delta`, so no negation belongs here.
    ///
    /// Separately, egui's own sign already matches `WheelDelta`'s, confirmed
    /// from `egui-0.34.1/src/containers/scroll_area.rs`: a `ScrollArea` moves
    /// by `state.offset[d] -= scroll_delta` (`scroll_delta` being
    /// `smooth_scroll_delta`), and `state.offset` is "how far scrolled past
    /// the top/left" (it feeds `Rect::from_min_size(inner_rect.min -
    /// state.offset, ..)` for the content rect). A positive
    /// `smooth_scroll_delta.y` therefore *decreases* that offset — moves the
    /// viewport back toward the top, i.e. "scrolls up, revealing more content
    /// above" — the exact same sentence `WheelDelta::y`'s doc comment uses.
    /// So egui's delta passes straight through, scaled to device pixels; see
    /// `scroll_to_wheel_delta_does_not_negate_egui_s_sign` below, which pins
    /// this down mechanically.
    fn scroll_to_wheel_delta(scroll: egui::Vec2, dpi: f32) -> WheelDelta {
        WheelDelta {
            x: (scroll.x * dpi) as f64,
            y: (scroll.y * dpi) as f64,
            z: 0.0,
            mode: WheelMode::DeltaPixel,
        }
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

/// A convenience for embedding a view with `ui.add(&mut view)` instead of
/// `view.show(ui)`.
///
/// This drops the [`WebViewEvent`]s [`WebView::show`] would have returned --
/// `egui::Widget::ui` can only hand back a [`egui::Response`], with no room
/// for a second value. Call [`WebView::show`] directly instead whenever the
/// caller needs navigation/lifecycle events (link clicks, title changes,
/// etc.); this impl exists for the common case of a view that's just being
/// displayed.
impl egui::Widget for &mut WebView {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        self.show_impl(ui).0
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
        // Letter / digit keys carry no case or shift information here (A5 of
        // PLAN.md) — `egui::Key` alone can't distinguish "a" from "A" or "2"
        // from "@". Deliberately `Unidentified` rather than the guessed
        // lowercase character this used to return: `show()` now forwards
        // `egui::Event::Text` separately as `Key::Character`, which egui
        // derives from the same fully layout- and shift-resolved source
        // winit itself uses. `egui_key_to_code` below still maps these to
        // real physical `Code`s, which carry no case ambiguity to begin with.
        egui::Key::A
        | egui::Key::B
        | egui::Key::C
        | egui::Key::D
        | egui::Key::E
        | egui::Key::F
        | egui::Key::G
        | egui::Key::H
        | egui::Key::I
        | egui::Key::J
        | egui::Key::K
        | egui::Key::L
        | egui::Key::M
        | egui::Key::N
        | egui::Key::O
        | egui::Key::P
        | egui::Key::Q
        | egui::Key::R
        | egui::Key::S
        | egui::Key::T
        | egui::Key::U
        | egui::Key::V
        | egui::Key::W
        | egui::Key::X
        | egui::Key::Y
        | egui::Key::Z
        | egui::Key::Num0
        | egui::Key::Num1
        | egui::Key::Num2
        | egui::Key::Num3
        | egui::Key::Num4
        | egui::Key::Num5
        | egui::Key::Num6
        | egui::Key::Num7
        | egui::Key::Num8
        | egui::Key::Num9 => Key::Named(NamedKey::Unidentified),
        _ => Key::Named(NamedKey::Unidentified),
    }
}

/// Turn one `egui::Event::Text` string into the Down/Up `KeyboardEvent` pair
/// that represents typing it. `code` is `Unidentified`: a `Text` event
/// doesn't carry which physical key produced it (it may not even correspond
/// to one, e.g. IME commit or a pasted character), so there is nothing
/// honest to put there — `key` (the actual character) is what page form
/// handlers care about anyway.
fn text_to_keyboard_events(text: &str, modifiers: Modifiers) -> [KeyboardEvent; 2] {
    let make = |state: KeyState| {
        KeyboardEvent::new(keyboard_types::KeyboardEvent {
            state,
            key: keyboard_types::Key::Character(text.to_string()),
            code: keyboard_types::Code::Unidentified,
            location: Location::Standard,
            modifiers,
            repeat: false,
            is_composing: false,
        })
    };
    [make(KeyState::Down), make(KeyState::Up)]
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

/// Map Servo's [`ServoCursor`] (the page's CSS `cursor` request) onto
/// `egui::CursorIcon`. Exhaustive by construction — a new `Cursor` variant
/// upstream fails this match at compile time rather than silently falling
/// back to `Default`.
fn servo_cursor_to_egui_cursor_icon(cursor: ServoCursor) -> egui::CursorIcon {
    use egui::CursorIcon as E;
    match cursor {
        ServoCursor::None => E::None,
        ServoCursor::Default => E::Default,
        ServoCursor::Pointer => E::PointingHand,
        ServoCursor::ContextMenu => E::ContextMenu,
        ServoCursor::Help => E::Help,
        ServoCursor::Progress => E::Progress,
        ServoCursor::Wait => E::Wait,
        ServoCursor::Cell => E::Cell,
        ServoCursor::Crosshair => E::Crosshair,
        ServoCursor::Text => E::Text,
        ServoCursor::VerticalText => E::VerticalText,
        ServoCursor::Alias => E::Alias,
        ServoCursor::Copy => E::Copy,
        ServoCursor::Move => E::Move,
        ServoCursor::NoDrop => E::NoDrop,
        ServoCursor::NotAllowed => E::NotAllowed,
        ServoCursor::Grab => E::Grab,
        ServoCursor::Grabbing => E::Grabbing,
        ServoCursor::EResize => E::ResizeEast,
        ServoCursor::NResize => E::ResizeNorth,
        ServoCursor::NeResize => E::ResizeNorthEast,
        ServoCursor::NwResize => E::ResizeNorthWest,
        ServoCursor::SResize => E::ResizeSouth,
        ServoCursor::SeResize => E::ResizeSouthEast,
        ServoCursor::SwResize => E::ResizeSouthWest,
        ServoCursor::WResize => E::ResizeWest,
        ServoCursor::EwResize => E::ResizeHorizontal,
        ServoCursor::NsResize => E::ResizeVertical,
        ServoCursor::NeswResize => E::ResizeNeSw,
        ServoCursor::NwseResize => E::ResizeNwSe,
        ServoCursor::ColResize => E::ResizeColumn,
        ServoCursor::RowResize => E::ResizeRow,
        ServoCursor::AllScroll => E::AllScroll,
        ServoCursor::ZoomIn => E::ZoomIn,
        ServoCursor::ZoomOut => E::ZoomOut,
    }
}

/// Convert one `egui::ImeEvent` into the `InputEvent::Ime` payload Servo
/// expects, mirroring servoshell's Start/Update/End/Dismissed states.
/// `keyboard_types::CompositionState` only has three variants
/// (`Start`/`Update`/`End`) — `Dismissed` lives one level up, on embedder_traits'
/// own `ImeEvent` (`Composition(CompositionEvent)` vs. `Dismissed`), which is
/// why `Enabled`/`Disabled` map onto two different Rust types below rather
/// than both being a `CompositionState`.
fn egui_ime_to_servo_ime(event: egui::ImeEvent) -> ServoImeEvent {
    match event {
        egui::ImeEvent::Enabled => ServoImeEvent::Composition(CompositionEvent {
            state: CompositionState::Start,
            data: String::new(),
        }),
        egui::ImeEvent::Preedit(text) => ServoImeEvent::Composition(CompositionEvent {
            state: CompositionState::Update,
            data: text,
        }),
        egui::ImeEvent::Commit(text) => ServoImeEvent::Composition(CompositionEvent {
            state: CompositionState::End,
            data: text,
        }),
        egui::ImeEvent::Disabled => ServoImeEvent::Dismissed,
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

    /// A5 landed: letter/digit keys no longer guess a lowercase character —
    /// `egui::Key` alone carries no case/shift information, so a real
    /// character now comes from `egui::Event::Text` via
    /// `text_to_keyboard_events` instead (tested below). This mapping's job
    /// for these keys is just "no synthesized character", not "no key at
    /// all" — `egui_key_to_code` still gives the real physical `Code`.
    #[test]
    fn letter_and_digit_keys_no_longer_synthesize_a_lowercase_character() {
        use keyboard_types::{Key, NamedKey};
        assert_eq!(egui_key_to_keyboard_types(&egui::Key::A), Key::Named(NamedKey::Unidentified));
        assert_eq!(egui_key_to_keyboard_types(&egui::Key::Num2), Key::Named(NamedKey::Unidentified));
        // Physical Code mapping is untouched -- letters/digits carry no case
        // ambiguity there to begin with.
        assert_eq!(egui_key_to_code(&egui::Key::A), keyboard_types::Code::KeyA);
    }

    #[test]
    fn text_to_keyboard_events_produces_a_down_then_up_with_the_real_character() {
        use keyboard_types::{Key, KeyState};
        let [down, up] = text_to_keyboard_events("@", Modifiers::SHIFT);
        assert_eq!(down.event.state, KeyState::Down);
        assert_eq!(down.event.key, Key::Character("@".into()));
        assert!(down.event.modifiers.contains(Modifiers::SHIFT));
        assert_eq!(up.event.state, KeyState::Up);
        assert_eq!(up.event.key, Key::Character("@".into()));
    }

    #[test]
    fn text_to_keyboard_events_carries_multi_character_input_through_unsplit() {
        // e.g. an IME commit or a paste landing as one Text event -- this
        // isn't meant to split it into individual keystrokes.
        let [down, _] = text_to_keyboard_events("café", Modifiers::empty());
        assert_eq!(down.event.key, keyboard_types::Key::Character("café".into()));
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

    // ── wheel scroll sign convention (A5, PLAN.md) ───────────────────────────

    #[test]
    fn scroll_to_wheel_delta_does_not_negate_egui_s_sign() {
        // See `WebView::scroll_to_wheel_delta`'s doc comment for the full
        // derivation from the vendored source. Short version: egui's
        // `smooth_scroll_delta` and Servo's `WheelDelta` already use the same
        // sign (both: positive `y` = scroll up / reveal content above), so
        // this conversion is a straight scale-to-device-pixels with no
        // negation on either axis -- unlike the old `Scroll::Delta` path,
        // which needed one because `Scroll::Delta` is the *opposite*
        // convention (confirmed by Servo's own `-wheel_event.delta` when it
        // internally turns a wheel event into a `Scroll::Delta`).
        let delta = WebView::scroll_to_wheel_delta(egui::vec2(3.0, 7.0), 2.0);
        assert_eq!(delta.x, 6.0);
        assert_eq!(delta.y, 14.0);
        assert_eq!(delta.mode, WheelMode::DeltaPixel);
        assert_eq!(delta.z, 0.0);
    }

    #[test]
    fn scroll_to_wheel_delta_handles_negative_scroll_without_a_double_flip() {
        // A regression on this point would silently invert scroll direction
        // (see PLAN.md's A5 section on why this was deferred for so long) --
        // pin both signs, not just the positive case above.
        let delta = WebView::scroll_to_wheel_delta(egui::vec2(-4.0, -9.0), 1.5);
        assert_eq!(delta.x, -6.0);
        assert_eq!(delta.y, -13.5);
    }

    // ── cursor mapping ────────────────────────────────────────────────────────

    #[test]
    fn cursor_mapping_covers_pointer_and_resize_cursors() {
        assert_eq!(servo_cursor_to_egui_cursor_icon(ServoCursor::Pointer), egui::CursorIcon::PointingHand);
        assert_eq!(servo_cursor_to_egui_cursor_icon(ServoCursor::Default), egui::CursorIcon::Default);
        assert_eq!(servo_cursor_to_egui_cursor_icon(ServoCursor::Text), egui::CursorIcon::Text);
        // The two enums don't share naming conventions for diagonal/edge
        // resize cursors (`NeswResize` vs. `ResizeNeSw`, `EwResize` vs.
        // `ResizeHorizontal`) -- exercise a few of the least obvious ones.
        assert_eq!(servo_cursor_to_egui_cursor_icon(ServoCursor::NeswResize), egui::CursorIcon::ResizeNeSw);
        assert_eq!(servo_cursor_to_egui_cursor_icon(ServoCursor::EwResize), egui::CursorIcon::ResizeHorizontal);
        assert_eq!(servo_cursor_to_egui_cursor_icon(ServoCursor::NsResize), egui::CursorIcon::ResizeVertical);
    }

    // ── IME mapping ───────────────────────────────────────────────────────────

    #[test]
    fn ime_events_map_to_the_matching_composition_state() {
        assert!(matches!(
            egui_ime_to_servo_ime(egui::ImeEvent::Enabled),
            ServoImeEvent::Composition(CompositionEvent { state: CompositionState::Start, .. })
        ));

        match egui_ime_to_servo_ime(egui::ImeEvent::Preedit("ｎ".into())) {
            ServoImeEvent::Composition(CompositionEvent { state: CompositionState::Update, data }) => {
                assert_eq!(data, "ｎ");
            }
            other => panic!("expected Composition(Update), got {other:?}"),
        }

        match egui_ime_to_servo_ime(egui::ImeEvent::Commit("日本語".into())) {
            ServoImeEvent::Composition(CompositionEvent { state: CompositionState::End, data }) => {
                assert_eq!(data, "日本語");
            }
            other => panic!("expected Composition(End), got {other:?}"),
        }

        // `Dismissed` is embedder_traits' `ImeEvent::Dismissed`, a sibling of
        // `Composition(..)` rather than a fourth `CompositionState` -- see
        // this function's doc comment for why.
        assert!(matches!(
            egui_ime_to_servo_ime(egui::ImeEvent::Disabled),
            ServoImeEvent::Dismissed
        ));
    }

    // ── clipboard shortcut mapping ───────────────────────────────────────────

    // ── overlay scrollbar (issue #15) ────────────────────────────────────────

    #[test]
    fn no_thumb_when_content_fits_the_viewport() {
        // scrollHeight <= clientHeight: nothing to scroll, so no scrollbar.
        assert_eq!(WebView::scrollbar_thumb_metrics(0.0, 400.0, 400.0, 300.0), None);
        assert_eq!(WebView::scrollbar_thumb_metrics(0.0, 300.0, 400.0, 300.0), None);
    }

    #[test]
    fn no_thumb_for_degenerate_inputs() {
        assert_eq!(WebView::scrollbar_thumb_metrics(0.0, 2000.0, 0.0, 300.0), None);
        assert_eq!(WebView::scrollbar_thumb_metrics(0.0, 2000.0, 400.0, 0.0), None);
    }

    #[test]
    fn thumb_height_is_proportional_to_the_visible_fraction() {
        // 400 of 2000 visible (20%) on a 300pt track -> a 60pt thumb.
        let (top, height) = WebView::scrollbar_thumb_metrics(0.0, 2000.0, 400.0, 300.0).unwrap();
        assert_eq!(top, 0.0);
        assert_eq!(height, 60.0);
    }

    #[test]
    fn thumb_height_never_shrinks_below_the_minimum() {
        // 40 of 20000 visible (0.2%) would compute to a sub-pixel thumb --
        // must clamp to MIN_THUMB_HEIGHT instead of vanishing.
        let (_, height) = WebView::scrollbar_thumb_metrics(0.0, 20000.0, 40.0, 300.0).unwrap();
        assert_eq!(height, WebView::MIN_THUMB_HEIGHT);
    }

    #[test]
    fn thumb_position_tracks_scroll_fraction() {
        // 400 visible of 2000 total -> max_scroll = 1600, thumb_height = 60,
        // thumb_travel = 240. Halfway scrolled (800/1600) -> thumb at 120.
        let (top, height) = WebView::scrollbar_thumb_metrics(800.0, 2000.0, 400.0, 300.0).unwrap();
        assert_eq!(height, 60.0);
        assert_eq!(top, 120.0);
    }

    #[test]
    fn thumb_reaches_the_track_s_bottom_at_max_scroll() {
        let (top, height) = WebView::scrollbar_thumb_metrics(1600.0, 2000.0, 400.0, 300.0).unwrap();
        assert_eq!(top + height, 300.0);
    }

    #[test]
    fn scroll_top_for_thumb_center_is_the_inverse_of_the_thumb_metrics() {
        // Same page as the tests above: max_scroll = 1600, thumb_height = 60,
        // thumb_travel = 240, track spans logical y in [10, 310).
        let track_top = 10.0;
        let track_height = 300.0;
        let thumb_height = 60.0;

        // Dragging the thumb's center to the track's own top -> scrollTop 0.
        let at_top = WebView::scroll_top_for_thumb_center(
            track_top, track_top, track_height, thumb_height, 2000.0, 400.0,
        );
        assert_eq!(at_top, 0.0);

        // Dragging to the track's bottom -> clamped to the max scroll.
        let at_bottom = WebView::scroll_top_for_thumb_center(
            track_top + track_height,
            track_top,
            track_height,
            thumb_height,
            2000.0,
            400.0,
        );
        assert_eq!(at_bottom, 1600.0);

        // Pointer out of bounds (dragged above/below the track) clamps
        // rather than producing a negative or out-of-range scrollTop.
        let above = WebView::scroll_top_for_thumb_center(
            track_top - 500.0,
            track_top,
            track_height,
            thumb_height,
            2000.0,
            400.0,
        );
        assert_eq!(above, 0.0);
        let below = WebView::scroll_top_for_thumb_center(
            track_top + track_height + 500.0,
            track_top,
            track_height,
            thumb_height,
            2000.0,
            400.0,
        );
        assert_eq!(below, 1600.0);
    }

    #[test]
    fn js_value_to_scroll_info_parses_the_polled_array_shape() {
        let value = JSValue::Array(vec![
            JSValue::Number(123.5),
            JSValue::Number(2000.0),
            JSValue::Number(400.0),
        ]);
        assert_eq!(
            js_value_to_scroll_info(&value),
            Some(ScrollInfo { scroll_top: 123.5, scroll_height: 2000.0, client_height: 400.0 })
        );
    }

    #[test]
    fn js_value_to_scroll_info_rejects_any_other_shape() {
        assert_eq!(js_value_to_scroll_info(&JSValue::Undefined), None);
        assert_eq!(js_value_to_scroll_info(&JSValue::Number(1.0)), None);
        // Wrong length.
        assert_eq!(
            js_value_to_scroll_info(&JSValue::Array(vec![JSValue::Number(1.0), JSValue::Number(2.0)])),
            None
        );
        // Right length, wrong element type -- e.g. a page whose
        // `document.scrollingElement` is unexpectedly null and the script's
        // own guard didn't run as expected.
        assert_eq!(
            js_value_to_scroll_info(&JSValue::Array(vec![
                JSValue::Null,
                JSValue::Number(2000.0),
                JSValue::Number(400.0)
            ])),
            None
        );
    }

    #[test]
    fn clipboard_shortcut_keys_map_to_the_matching_editing_action() {
        // Mirrors the match in `show_impl`'s keyboard-event loop -- kept as a
        // small pure table here so the C/X/V -> Copy/Cut/Paste mapping has a
        // regression test independent of a live Servo view.
        fn action_for(key: egui::Key) -> Option<EditingActionEvent> {
            match key {
                egui::Key::C => Some(EditingActionEvent::Copy),
                egui::Key::X => Some(EditingActionEvent::Cut),
                egui::Key::V => Some(EditingActionEvent::Paste),
                _ => None,
            }
        }
        assert!(matches!(action_for(egui::Key::C), Some(EditingActionEvent::Copy)));
        assert!(matches!(action_for(egui::Key::X), Some(EditingActionEvent::Cut)));
        assert!(matches!(action_for(egui::Key::V), Some(EditingActionEvent::Paste)));
        assert!(action_for(egui::Key::A).is_none());
    }
}
