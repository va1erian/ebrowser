//! `egui-litehtml-webview` -- a reusable egui widget that renders HTML/CSS
//! message bodies via [litehtml](https://github.com/litehtml/litehtml)
//! (through the `va1erian/litehtml-rs` Rust bindings), replacing the earlier
//! `egui-servo-webview` (full Servo browser engine).
//!
//! # Why litehtml, not Servo
//!
//! No legitimate mail client executes JavaScript in HTML email, so carrying
//! a whole JS-capable browser engine just to lay out message bodies was
//! always more than the job needed. litehtml is a JS-less HTML/CSS layout
//! and rendering engine -- dropping the JS engine (and everything Servo
//! pulls in to support it) is what makes the resulting binary dramatically
//! smaller. See `PLAN.md`'s migration section for the full rationale and the
//! measured before/after binary size.
//!
//! # Design: no persisted `litehtml::Document`
//!
//! `litehtml::Document<'a>` borrows its `DocumentContainer` mutably for the
//! `Document`'s own lifetime, which makes storing both a `Document` and its
//! backing container as sibling fields of one long-lived struct a
//! self-referential-struct problem. This crate sidesteps that entirely:
//! [`WebView`] stores only the owned [`litehtml::pixbuf::PixbufContainer`]
//! (persistent, reused across frames -- it is what actually holds the
//! rendered pixels) and the current HTML string. A `Document` is
//! constructed fresh, used, and dropped every time layout or drawing is
//! actually needed (on load/reload, on a resize, or to hit-test a click) --
//! never stored as a field. litehtml's parse+layout for mail-sized HTML is
//! fast enough that this "reload on every re-render" model is the right,
//! simple design, matching how this crate's Servo-backed predecessor's
//! `WebView::load()` already worked (throw away the old page, load fresh).
//!
//! # Render sequence
//!
//! Because the container's pixel buffer must be sized to match the
//! document's actual content height before the final draw, showing a
//! message is a multi-pass sequence (see [`WebView::show`] / its private
//! `relayout`):
//!
//! 1. **Measure**: build a `Document`, `render()` it at the widget's current
//!    width, read `height()`, then drop it.
//! 2. **Resize** the container's pixel buffer to that height (this clears
//!    its pixel content, per `PixbufContainer::resize`'s own doc comment --
//!    which is exactly why step 3 below re-renders and re-draws rather than
//!    reusing anything from step 1).
//! 3. **Draw**: build a *new* `Document` (layout state does not survive a
//!    container resize), `render()` + `draw()` it into the now-correctly-sized
//!    buffer.
//! 4. **Resolve images**: drain `take_pending_images()` -- URLs the layout
//!    discovered are unknown until this point, since they are only found by
//!    walking the parsed document. `data:` URIs are decoded locally
//!    (litehtml has no network layer and does not do this itself outside
//!    its own `prepare_html` pipeline, which this crate deliberately does
//!    not use -- see the note on sanitization below); `http(s)` URLs are
//!    handed to the host's [`WebViewHandler::intercept`], which decides
//!    whether to fetch and returns the bytes via
//!    [`InterceptOutcome::Serve`]. Any image actually loaded means steps 1-3
//!    run one more time so it actually appears (an image can also change the
//!    document's content height, hence remeasuring, not just redrawing).
//!
//! # Sanitization stays the host's job
//!
//! litehtml's `email` feature includes its own `prepare_html`/
//! `prepare_email_html` pipeline with script-stripping sanitization. This
//! crate does not use it. `esmail`'s `render.rs` already runs the message
//! through `ammonia` (a dedicated, well-audited HTML sanitizer) before any
//! of this crate's code sees it, and that stays the real trust boundary --
//! defense in depth, not replaced by litehtml's own safety net, per the
//! approved migration plan.

#![warn(missing_docs)]

pub use url;

use std::cell::RefCell;
use std::rc::Rc;

use litehtml::email::EMAIL_MASTER_CSS;
use litehtml::html::decode_data_uri;
use litehtml::pixbuf::PixbufContainer;
use litehtml::{Document, DrawContext};

/// Seed/fallback viewport height (logical points) used for `vh`-unit CSS
/// resolution.
///
/// **Known limitation:** this crate renders a message body as one static
/// image at its full content height (so [`WebView::show`]'s `ScrollArea` can
/// scroll it natively), not into a fixed-size viewport the way a real
/// browser window is. `PixbufContainer` has no separate "viewport size" from
/// "canvas size" -- both come from the same `resize_with_scale` call -- so
/// `vh` units end up relative to *this render's own content height*, not a
/// stable window size. A document seeded at height 1 (this crate's very
/// first render, before anything has been measured) would resolve `1vh` to
/// ~0.01px, collapsing any `height: NNvh` block to nothing -- a real bug
/// this constant exists to avoid, caught by the mandatory
/// `ESMAIL_PREVIEW=demo` screenshot check (see HANDOFF.md §2) against the
/// demo page's own `.tall { height: 60vh; }` block. Seeding with a plausible
/// window height instead means `vh` at least resolves to something
/// reasonable rather than collapsing to zero; it does not make `vh` mean
/// what it would in a real browser. In practice this is a non-issue for
/// real mail: no mainstream mail client preserves or predictably renders
/// viewport-relative units in HTML email, so authors do not rely on them.
const DEFAULT_VIEWPORT_HEIGHT: u32 = 800;

// ─── Public API types ───────────────────────────────────────────────────────

/// What to load in the webview.
///
/// Only an in-memory HTML string is supported. The prior Servo-backed crate
/// also had `Url` (navigate the engine directly) and `HtmlWithBase`
/// (resolve relative links/resources against a base URL) variants; neither
/// is needed here. litehtml has no network layer of its own by design, so
/// there is nothing "navigate to a URL" could mean at this layer -- the one
/// caller that wants that (`esmail`'s `ESMAIL_PREVIEW=<http url>` dev path)
/// fetches synchronously with `ureq` and hands the result in as `Html`
/// instead. Nothing in `esmail` today uses relative links/resources against
/// a non-trivial base, so `HtmlWithBase` was dropped rather than ported
/// speculatively; both can come back if something real needs them.
#[derive(Clone)]
pub enum WebViewSource {
    /// Render an in-memory HTML string.
    Html(String),
}

/// Events emitted by [`WebView::show`].
#[derive(Debug, Clone)]
pub enum WebViewEvent {
    /// The user clicked a link (an `<a href>`). litehtml has no navigation
    /// concept of its own -- there is nothing to allow/deny -- so every
    /// anchor click unconditionally becomes this event; the host is always
    /// the one that decides what to do with it (open in the system browser,
    /// etc.), same as how the Servo-backed predecessor's `MessageViewHandler`
    /// always denied in-view navigation and reported it this way too.
    LinkClicked(String),
}

/// One resource load litehtml's layout discovered it wants: an image `src`
/// found while parsing/laying out the document. Deliberately minimal --
/// litehtml hands this crate just a URL string per pending image, not a
/// full HTTP request object the way Servo's `WebResourceRequest` did, so
/// there is no request method/headers/redirect info to carry here.
pub struct ImageRequest {
    /// The image URL as it appeared in the document's markup (already
    /// resolved from `cid:` to a `data:` URL upstream by `esmail`'s
    /// `render.rs`, for any part that had a match -- see the crate's module
    /// doc. `data:` URLs never reach [`WebViewHandler::intercept`] at all;
    /// this crate decodes those itself. Only `http(s)` (or any other
    /// non-local scheme) URLs are handed to the handler.
    pub url: String,
}

/// What [`WebViewHandler::intercept`] decided to do with one pending image.
pub enum InterceptOutcome {
    /// Do not fetch this image. Functionally identical to [`Self::Block`]
    /// today (this crate has no network layer of its own to fall back to),
    /// kept as a distinct outcome for symmetry with the request/response
    /// shape and in case a default fetcher is ever added later.
    Allow,
    /// Do not fetch this image; it is simply left unloaded (no broken-image
    /// placeholder is drawn -- litehtml just never gets pixels for it).
    Block,
    /// Serve these bytes as the image's data, fetched however the host saw
    /// fit (e.g. `esmail`'s `MessageViewHandler` uses `ureq` once the user
    /// has clicked "Load remote images" -- see B5 in PLAN.md).
    Serve(Vec<u8>),
}

/// Host-supplied policy for which images a [`WebView`] is allowed to load.
///
/// Unlike the Servo-backed predecessor's `WebViewHandler`, there is no
/// `navigation` method: litehtml has no navigation concept at all (see
/// [`WebViewEvent::LinkClicked`]'s doc), so there is nothing left to decide
/// there.
pub trait WebViewHandler {
    /// Called for every image URL the document's layout wants loaded, other
    /// than `data:` URLs (decoded locally, never reaching this hook -- see
    /// [`ImageRequest::url`]). Defaults to [`InterceptOutcome::Allow`] (no
    /// fetch), matching this crate having no default image fetcher.
    fn intercept(&mut self, request: &ImageRequest) -> InterceptOutcome {
        let _ = request;
        InterceptOutcome::Allow
    }
}

/// The [`WebViewHandler`] used when a [`WebViewConfig`] does not supply one:
/// no image is ever fetched.
struct DefaultHandler;
impl WebViewHandler for DefaultHandler {}

// ─── WebViewHost ─────────────────────────────────────────────────────────────

/// Creates [`WebView`]s.
///
/// Dramatically simpler than the Servo-backed predecessor's `WebViewHost`:
/// litehtml's `pixbuf` backend renders entirely on the CPU, so there is no
/// engine to own, no window handle, and no GL context to set up. This type
/// still exists (rather than a bare associated function on `WebView`) to
/// keep the call shape `esmail`'s `main.rs` already uses -- one host per
/// window, producing any number of views -- even though today it is little
/// more than an id counter so two views in the same window don't collide on
/// one egui texture name.
#[derive(Default)]
pub struct WebViewHost {
    next_view_id: std::cell::Cell<u64>,
}

impl WebViewHost {
    /// Create a host. Takes nothing: unlike the Servo-backed predecessor's
    /// `WebViewHost::new`/`from_eframe` (which needed a window/display
    /// handle to set up a rendering context), litehtml's CPU-only `pixbuf`
    /// backend needs nothing from the host window at all.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new view showing `config.source`.
    pub fn new_view(&self, ctx: &egui::Context, config: WebViewConfig) -> WebView {
        let view_id = self.next_view_id.get();
        self.next_view_id.set(view_id + 1);

        let WebViewSource::Html(html) = config.source;
        let scale = ctx.pixels_per_point();

        WebView {
            // 1x1 placeholder; the first `show()` call resizes this to the
            // widget's actual width before anything is measured or drawn.
            // Height seeded to a plausible viewport size, not 1px -- see
            // `DEFAULT_VIEWPORT_HEIGHT`'s doc for why a degenerate initial
            // height is a real, visible bug for any message using `vh`
            // units, not just a cosmetic nit.
            container: PixbufContainer::new_with_scale(1, DEFAULT_VIEWPORT_HEIGHT, scale),
            html,
            handler: config
                .handler
                .unwrap_or_else(|| Rc::new(RefCell::new(DefaultHandler))),
            texture: None,
            texture_name: format!("egui_litehtml_webview_{view_id}"),
            logical_width: 1.0,
            content_height: DEFAULT_VIEWPORT_HEIGHT as f32,
            pixels_per_point: scale,
            dirty: true,
        }
    }
}

// ─── WebView ─────────────────────────────────────────────────────────────────

/// How a view should start up.
pub struct WebViewConfig {
    /// The page to load first.
    pub source: WebViewSource,
    /// Which images this view is allowed to load. `None` uses
    /// [`DefaultHandler`]'s behaviour: no image is ever fetched.
    pub handler: Option<Rc<RefCell<dyn WebViewHandler>>>,
}

impl WebViewConfig {
    /// A config that loads `source`, with the default (no images fetched)
    /// policy.
    pub fn new(source: WebViewSource) -> Self {
        Self { source, handler: None }
    }

    /// Use `handler` for this view's image-loading decisions instead of the
    /// default policy.
    pub fn with_handler(mut self, handler: Rc<RefCell<dyn WebViewHandler>>) -> Self {
        self.handler = Some(handler);
        self
    }
}

/// One embedded HTML view, drawn with [`WebView::show`].
///
/// Created by [`WebViewHost::new_view`].
pub struct WebView {
    /// Owns the rendered pixels. Persistent across frames/loads -- see the
    /// crate module doc for why no `litehtml::Document` is ever stored
    /// alongside it.
    container: PixbufContainer,
    /// The HTML currently loaded. Kept so a resize (which requires a fresh
    /// `Document`, see the module doc) can re-parse without the caller
    /// having to `load()` again.
    html: String,
    handler: Rc<RefCell<dyn WebViewHandler>>,
    /// Reused across frames; reallocating one per frame was measurable waste
    /// in the predecessor crate and would be here too.
    texture: Option<egui::TextureHandle>,
    /// Unique per view, so two views cannot collide on one egui texture.
    texture_name: String,
    /// The width `relayout` last laid out at, in egui logical points.
    logical_width: f32,
    /// The document's content height after the last `relayout`, in egui
    /// logical points -- what [`WebView::show`] sizes the displayed image
    /// to.
    content_height: f32,
    /// `egui::Context::pixels_per_point` last laid out at.
    pixels_per_point: f32,
    /// Set by [`WebView::load`]/[`WebView::reload`] and by a width/DPI
    /// change noticed in `show()`; cleared once `relayout` has run. Keeps a
    /// `show()` on an unchanged view cheap (no reparse/relayout), matching
    /// the predecessor crate's frame-dirty gating in spirit even though the
    /// underlying engine is completely different.
    dirty: bool,
}

impl WebView {
    // ── Public API ──────────────────────────────────────────────────────────

    /// Load a new source, replacing whatever is currently shown. Triggers a
    /// fresh render on the next [`WebView::show`].
    pub fn load(&mut self, source: WebViewSource) {
        let WebViewSource::Html(html) = source;
        self.html = html;
        // New page: image URLs from the old one should not suppress
        // re-discovery, even if by coincidence a URL string repeats.
        self.container.clear_pending_images();
        self.dirty = true;
    }

    /// Re-run the render sequence for the currently-loaded HTML.
    ///
    /// Used by `esmail`'s "Load remote images" button: the HTML itself never
    /// loses its original `http(s)` URLs (B5 in PLAN.md), so once
    /// `MessageViewHandler::allow_remote` flips, a `reload()` against the
    /// same document is what actually re-requests them -- `clear_pending_images`
    /// is required for that, since without it every URL would still be
    /// marked "already requested" from the blocked first pass and never
    /// make it back into `take_pending_images()`.
    pub fn reload(&mut self) {
        self.container.clear_pending_images();
        self.dirty = true;
    }

    /// Draw the view into `ui` (inside its own scroll area) and return any
    /// queued [`WebViewEvent`]s.
    ///
    /// Call once per frame. Re-parses and re-lays-out only when something
    /// actually changed (a `load`/`reload`, or the widget's width/DPI) --
    /// see [`WebView::dirty`].
    pub fn show(&mut self, ui: &mut egui::Ui) -> Vec<WebViewEvent> {
        let dpi = ui.ctx().pixels_per_point();
        let avail_width = ui.available_width().max(1.0);

        if self.dirty
            || (avail_width - self.logical_width).abs() > 0.5
            || (dpi - self.pixels_per_point).abs() > 0.001
        {
            self.logical_width = avail_width;
            self.pixels_per_point = dpi;
            self.relayout(ui.ctx());
        }

        let mut events = Vec::new();
        // The whole document is rendered up front at its full content
        // height into one texture -- unlike the Servo-backed predecessor,
        // which had to poll a JS bridge and paint a hand-rolled overlay
        // scrollbar (issue #15) because Servo exposed no scroll-position
        // getter/setter at all. A plain `ScrollArea` around a normally-sized
        // `Image` gets a native scrollbar and native wheel-scroll for free.
        egui::ScrollArea::vertical()
            .id_salt(&self.texture_name)
            .show(ui, |ui| {
                let Some(texture) = &self.texture else {
                    return;
                };
                let size = egui::vec2(self.logical_width, self.content_height);
                let sized = egui::load::SizedTexture::new(texture.id(), size);
                let resp = ui.add(egui::Image::from_texture(sized).sense(egui::Sense::click()));

                if resp.clicked() {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        let doc_x = pos.x - resp.rect.left();
                        let doc_y = pos.y - resp.rect.top();
                        if let Some(url) = self.hit_test_anchor(doc_x, doc_y) {
                            events.push(WebViewEvent::LinkClicked(url));
                        }
                    }
                }
            });

        events
    }

    // ── Private: render sequence ──────────────────────────────────────────

    /// Run the full measure -> resize -> draw -> resolve-images sequence
    /// described in the crate module doc.
    fn relayout(&mut self, ctx: &egui::Context) {
        let width = self.logical_width.max(1.0);
        let scale = self.pixels_per_point.max(0.1);

        let height = self.layout_only(width).unwrap_or(1.0);
        self.resize_container(width, height, scale);
        self.content_height = self.layout_and_draw(width).unwrap_or(height);

        if self.load_pending_images() {
            let height2 = self.layout_only(width).unwrap_or(self.content_height);
            self.resize_container(width, height2, scale);
            self.content_height = self.layout_and_draw(width).unwrap_or(height2);
        }

        self.upload_texture(ctx);
        self.dirty = false;
    }

    /// Parse + lay out (no draw) and return the content height. Used both
    /// for the initial measurement pass and to re-measure after images
    /// load (an image without explicit dimensions can change the
    /// document's height).
    fn layout_only(&mut self, width: f32) -> Option<f32> {
        let mut doc = self.parse()?;
        let _ = doc.render(width);
        Some(doc.height().max(1.0))
    }

    /// Parse + lay out + draw into `self.container`'s current pixel buffer.
    /// Returns the content height.
    fn layout_and_draw(&mut self, width: f32) -> Option<f32> {
        let mut doc = self.parse()?;
        let _ = doc.render(width);
        let height = doc.height().max(1.0);
        doc.draw(DrawContext::default(), 0.0, 0.0, None);
        Some(height)
    }

    /// Parse `self.html` into a fresh `Document` borrowing `self.container`.
    /// See the crate module doc for why this is never stored.
    ///
    /// `master_css: None` is deliberate, not an oversight: the vendored
    /// litehtml C++ core only falls back to its own **built-in** master
    /// stylesheet (which is where `<h1>`/`<p>`/`<div>`/`<table>`/etc. get
    /// their default `display: block`/`table-row`/etc. — see
    /// `litehtml_c.cpp`'s `lh_document_create_from_string`) when the
    /// `master_css` argument is null. Passing `Some(EMAIL_MASTER_CSS)` there
    /// *replaces* the built-in stylesheet outright rather than layering on
    /// top of it, which was tried first and produced a real, visible bug:
    /// every element collapsed onto one or two inline-flowed lines (caught
    /// by the mandatory `ESMAIL_PREVIEW=demo` screenshot check, exactly as
    /// HANDOFF.md warns this kind of thing can compile clean and pass every
    /// test). `EMAIL_MASTER_CSS` (margin/table/link resets suited to email)
    /// belongs as `user_styles` instead, which litehtml documents as applied
    /// *after* the built-in master and the document's own styles — still
    /// low enough specificity (plain type selectors) that a message's own
    /// inline `style="..."` attributes win where they conflict.
    fn parse(&mut self) -> Option<Document<'_>> {
        match Document::from_html(&self.html, &mut self.container, None, Some(EMAIL_MASTER_CSS)) {
            Ok(doc) => Some(doc),
            Err(e) => {
                log::warn!("egui-litehtml-webview: failed to parse message HTML: {e}");
                None
            }
        }
    }

    /// Resize the pixel buffer to `width` x `height` (logical points) at
    /// `scale`. Clears existing pixel content -- callers must draw again
    /// afterwards.
    fn resize_container(&mut self, width: f32, height: f32, scale: f32) {
        let w = width.ceil().max(1.0) as u32;
        let h = height.ceil().max(1.0) as u32;
        self.container.resize_with_scale(w, h, scale);
    }

    /// Drain pending image URLs and resolve as many as possible. Returns
    /// whether any image was actually loaded (i.e. whether a re-render is
    /// worth doing).
    fn load_pending_images(&mut self) -> bool {
        let pending = self.container.take_pending_images();
        if pending.is_empty() {
            return false;
        }
        let mut any_loaded = false;
        for (url, _redraw_on_ready) in pending {
            if let Some(bytes) = resolve_image_bytes(&url, &mut *self.handler.borrow_mut()) {
                self.container.load_image_data(&url, &bytes);
                any_loaded = true;
            }
        }
        any_loaded
    }

    /// Copy `self.container`'s pixels into the reused egui texture.
    ///
    /// `PixbufContainer::pixels`'s own doc comment says it returns
    /// **premultiplied** RGBA -- confirmed against `pixbuf.rs`'s
    /// `load_image_data` (which explicitly premultiplies incoming image
    /// bytes before storing them) and its `blend_pixel` helper (which
    /// documents "the pixmap stores premultiplied RGBA").
    ///
    /// `PixbufContainer::new_with_scale`'s own doc comment says it
    /// "initializes a transparent pixmap" -- unlike a real browser (or the
    /// Servo-backed predecessor, which always painted an opaque white
    /// canvas), litehtml only paints where CSS actually says to. A message
    /// with no explicit `body { background }` (the overwhelming common
    /// case) would otherwise show whatever egui panel color sits behind
    /// the texture bleeding through every unpainted region -- caught by
    /// the mandatory `ESMAIL_PREVIEW=demo` screenshot check against a dark
    /// theme, where the demo page's plain text was rendered dark-on-dark
    /// instead of dark-on-white. Fixed by flattening onto opaque white
    /// ourselves before upload, rather than trying to get litehtml to
    /// paint a canvas background it has no concept of. Compositing
    /// premultiplied-alpha `src` over opaque white simplifies to
    /// `out = src_channel + (255 - alpha)` per channel (the general "over"
    /// formula's `(1-src_a)*bg` term collapses since `bg == 255`), so this
    /// needs no general alpha-blend math, just one add per byte.
    fn upload_texture(&mut self, ctx: &egui::Context) {
        let w = self.container.width() as usize;
        let h = self.container.height() as usize;
        if w == 0 || h == 0 {
            return;
        }
        let mut flattened = self.container.pixels().to_vec();
        for px in flattened.chunks_exact_mut(4) {
            let alpha = px[3];
            if alpha == 255 {
                continue;
            }
            let carry = 255 - alpha;
            px[0] = px[0].saturating_add(carry);
            px[1] = px[1].saturating_add(carry);
            px[2] = px[2].saturating_add(carry);
            px[3] = 255;
        }
        let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], &flattened);
        match &mut self.texture {
            Some(handle) => handle.set(color_image, egui::TextureOptions::LINEAR),
            slot => {
                let _ = slot.insert(ctx.load_texture(
                    &self.texture_name,
                    color_image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
    }

    /// Build one more short-lived `Document`, feed it a down+up click at
    /// document-local coordinates `(x, y)` (logical points, i.e. the same
    /// space `render()` was called with), and return the anchor URL if that
    /// completed a click on a link. Cheaper than a full render pass: layout
    /// alone is enough for litehtml's own hit-testing, no `draw()` needed.
    fn hit_test_anchor(&mut self, x: f32, y: f32) -> Option<String> {
        let width = self.logical_width.max(1.0);
        let mut doc = self.parse()?;
        let _ = doc.render(width);
        doc.on_lbutton_down(x, y, x, y);
        doc.on_lbutton_up(x, y, x, y);
        drop(doc);
        self.container.take_anchor_click()
    }
}

/// Decide how to resolve one pending image URL, without touching the
/// container -- pulled out of [`WebView::load_pending_images`] so it's
/// testable without a real [`PixbufContainer`]/`Document`. `data:` URLs are
/// decoded locally; a surviving `cid:` URL means `esmail`'s `render.rs`
/// found no matching part and there's nothing to fetch (see
/// [`ImageRequest::url`]'s doc); anything else goes to `handler`.
fn resolve_image_bytes(url: &str, handler: &mut dyn WebViewHandler) -> Option<Vec<u8>> {
    if let Some(data) = decode_data_uri(url) {
        return Some(data);
    }
    if url.starts_with("cid:") {
        return None;
    }
    let request = ImageRequest { url: url.to_string() };
    match handler.intercept(&request) {
        InterceptOutcome::Serve(bytes) => Some(bytes),
        InterceptOutcome::Allow | InterceptOutcome::Block => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingHandler {
        seen: Vec<String>,
        outcome: InterceptOutcome,
    }

    impl WebViewHandler for RecordingHandler {
        fn intercept(&mut self, request: &ImageRequest) -> InterceptOutcome {
            self.seen.push(request.url.clone());
            match &self.outcome {
                InterceptOutcome::Allow => InterceptOutcome::Allow,
                InterceptOutcome::Block => InterceptOutcome::Block,
                InterceptOutcome::Serve(bytes) => InterceptOutcome::Serve(bytes.clone()),
            }
        }
    }

    #[test]
    fn resolve_image_bytes_decodes_a_data_uri_without_asking_the_handler() {
        // "hi" base64-encoded, arbitrary content -- only the round trip
        // through decode_data_uri matters here.
        let mut handler = RecordingHandler { seen: Vec::new(), outcome: InterceptOutcome::Allow };
        let bytes = resolve_image_bytes("data:text/plain;base64,aGk=", &mut handler);
        assert_eq!(bytes, Some(b"hi".to_vec()));
        assert!(handler.seen.is_empty(), "a data: URL must never reach the handler");
    }

    #[test]
    fn resolve_image_bytes_leaves_an_unmatched_cid_unresolved_without_asking_the_handler() {
        // render.rs (B5) already inlines every cid: part it can match as a
        // data: URL before the HTML reaches this crate -- a cid: surviving
        // to here means no match was found, and there's nothing to fetch.
        let mut handler = RecordingHandler { seen: Vec::new(), outcome: InterceptOutcome::Serve(vec![1]) };
        let bytes = resolve_image_bytes("cid:missing-part", &mut handler);
        assert_eq!(bytes, None);
        assert!(handler.seen.is_empty(), "an unmatched cid: URL must never reach the handler either");
    }

    #[test]
    fn resolve_image_bytes_asks_the_handler_for_a_remote_url_and_serves_its_bytes() {
        let mut handler = RecordingHandler { seen: Vec::new(), outcome: InterceptOutcome::Serve(vec![9, 9, 9]) };
        let bytes = resolve_image_bytes("https://example.com/pixel.png", &mut handler);
        assert_eq!(bytes, Some(vec![9, 9, 9]));
        assert_eq!(handler.seen, vec!["https://example.com/pixel.png".to_string()]);
    }

    #[test]
    fn resolve_image_bytes_blocks_a_remote_url_when_the_handler_declines() {
        for outcome in [InterceptOutcome::Allow, InterceptOutcome::Block] {
            let mut handler = RecordingHandler { seen: Vec::new(), outcome };
            let bytes = resolve_image_bytes("https://example.com/track.gif", &mut handler);
            assert_eq!(bytes, None);
        }
    }
}
