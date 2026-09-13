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
    WebViewPoint, WebViewVector, WindowRenderingContext, NavigationRequest,
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
    /// Render an in-memory HTML string.
    Html(String),
}

/// Events emitted by [`WebView::show`].
#[derive(Debug, Clone)]
pub enum WebViewEvent {
    /// The user triggered a navigation to a new URL (link click, etc.).
    LinkClicked(String),
}

// ─── Internal delegate ───────────────────────────────────────────────────────

struct Delegate {
    egui_ctx: egui::Context,
    /// Events queued during `request_navigation`; drained by `show()` each frame.
    events: Rc<RefCell<Vec<WebViewEvent>>>,
    /// Track whether the very first load has been dispatched so we can
    /// distinguish the initial navigation from user-initiated link clicks.
    initial_load_done: Rc<RefCell<bool>>,
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _webview: ServoWebView) {
        self.egui_ctx.request_repaint();
    }

    fn request_navigation(&self, _webview: ServoWebView, request: NavigationRequest) {
        let mut done = self.initial_load_done.borrow_mut();
        if *done {
            // User navigated away – report it and let the host decide.
            //
            // `deny()` is required, not optional: servo's `NavigationRequest`
            // fails OPEN. Its `Drop` impl sends "allow", so simply dropping the
            // request here let the navigation proceed *and* emitted a
            // LinkClicked event, so a link opened in the webview and in the
            // system browser at once. A3 replaces this with a host-supplied
            // policy; until then, deny and surface the event.
            self.events
                .borrow_mut()
                .push(WebViewEvent::LinkClicked(request.url.to_string()));
            request.deny();
        } else {
            *done = true;
            // Allow initial navigation
            request.allow();
        }
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

        let delegate = Rc::new(Delegate {
            egui_ctx: egui_ctx.clone(),
            events: events.clone(),
            initial_load_done,
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
}

impl WebViewConfig {
    /// A config that loads `source` at a default size.
    pub fn new(source: WebViewSource) -> Self {
        Self {
            source,
            size: PhysicalSize::new(1024, 768),
        }
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

    /// Draw the view into `ui` and return any queued [`WebViewEvent`]s.
    ///
    /// Call once per frame per view. The engine itself is driven separately by
    /// [`WebViewHost::spin`], which must be called once per frame overall.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Vec<WebViewEvent> {
        let available = ui.available_size();
        let dpi = ui.ctx().pixels_per_point();

        // Physical pixel size for this frame.
        let phys_w = ((available.x * dpi) as u32).max(1);
        let phys_h = ((available.y * dpi) as u32).max(1);
        let phys_size = PhysicalSize::new(phys_w, phys_h);

        if phys_size != self.last_phys_size {
            self.servo_view.resize(phys_size);
            self.servo_view.set_hidpi_scale_factor(
                Scale::<f32, DeviceIndependentPixel, DevicePixel>::new(dpi),
            );
            self.last_phys_size = phys_size;
        }

        // Paint Servo's current frame into the offscreen framebuffer.
        self.servo_view.paint();

        // Allocate the widget rect before drawing.
        let resp = ui.allocate_rect(
            ui.available_rect_before_wrap(),
            egui::Sense::click_and_drag(),
        );
        let widget_rect = resp.rect;

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
                let dp = self.egui_to_servo_point(pos, widget_rect.min, dpi);
                
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
                    let dp = self.egui_to_servo_point(pos, widget_rect.min, dpi);
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
            let center_pt = self.egui_to_servo_point(widget_rect.center(), widget_rect.min, dpi);
            let scroll_pt = if widget_rect.contains(hover) {
                self.egui_to_servo_point(hover, widget_rect.min, dpi)
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
            let center = self.egui_to_servo_point(widget_rect.center(), widget_rect.min, dpi);

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

    /// Convert an egui logical-pixel position to a Servo device-pixel `WebViewPoint`,
    /// relative to the top-left corner of the webview widget.
    fn egui_to_servo_point(
        &self,
        pos: egui::Pos2,
        origin: egui::Pos2,
        dpi: f32,
    ) -> WebViewPoint {
        use euclid::Point2D;
        let x = (pos.x - origin.x) * dpi;
        let y = (pos.y - origin.y) * dpi;
        WebViewPoint::Device(Point2D::new(x, y))
    }

    /// Convert a [`WebViewSource`] into a [`Url`] Servo can load.
    /// `Html` is base64-encoded into a `data:` URL so no server is needed.
    fn source_to_url(source: &WebViewSource) -> Url {
        match source {
            WebViewSource::Url(u) => Url::parse(u).unwrap_or_else(|_| {
                Url::parse("about:blank").expect("about:blank is always valid")
            }),
            WebViewSource::Html(html) => {
                let b64 = general_purpose::STANDARD.encode(html.as_bytes());
                let s = format!("data:text/html;charset=utf-8;base64,{}", b64);
                Url::parse(&s).expect("data URL is always valid")
            }
        }
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
