# egui-servo-webview

An `egui` widget that embeds the [Servo](https://servo.org) browser engine.

**Internal to this workspace.** Not published to crates.io -- see PLAN.md's A7
section. "Reusable" here means a clean boundary another crate in this
workspace can depend on, not a public, semver-stable API. The mail client
(`crates/esmail`) is this crate's real-world demo and integration test; the
`examples/two_views.rs` example exists only to prove the multi-instance split
described below still holds, not as a polished browser.

## The `WebViewHost` / `WebView` split

- **`WebViewHost`** owns the Servo engine and the window's rendering context.
  Create **one per window**, from `WebViewHost::new` (generic over
  `raw-window-handle`) or, with the `eframe` feature (on by default),
  `WebViewHost::from_eframe(cc, size)`.
- **`WebView`** is one embedded page. Create any number of them from one host
  with `host.new_view(&egui_ctx, WebViewConfig::new(source))`; each gets its
  own offscreen rendering context, navigation state, and input focus, but all
  share the one underlying engine -- a second view costs a rendering context,
  not a second browser engine. `examples/two_views.rs` demonstrates two
  independent views side by side.

Every frame:

```rust,no_run
# use egui_servo_webview::{WebViewHost, WebViewConfig, WebViewSource};
# use dpi::PhysicalSize;
# fn frame(host: &WebViewHost, view: &mut egui_servo_webview::WebView, ui: &mut egui::Ui) {
host.spin();           // drive the engine once, however many views exist
let events = view.show(ui);  // draw this one view, once
for event in events {
    // handle WebViewEvent::LinkClicked, TitleChanged, etc.
}
# }
```

`WebView` also implements `egui::Widget` (`ui.add(&mut view)`), for the common
case of a view that's just being displayed and doesn't need its
`WebViewEvent`s -- see that impl's doc comment for the trade-off.

## Navigation and resource policy

A view's owner supplies a `WebViewHandler` (via `WebViewConfig::with_handler`)
to decide what happens on navigation past the initial load, and what to do
with every resource the page tries to fetch (allow it, block it outright, or
serve substitute bytes -- e.g. resolving a mail `cid:` part from memory). Both
underlying Servo hooks **fail open**: dropping a `NavigationRequest` allows the
navigation, and an unhandled resource load lets it through unmodified. See the
doc comment on `WebViewHandler` and the landmine recorded in this repo's
`HANDOFF.md` §3.1 -- this already shipped as a real bug once.

There is no `stop()`: `servo::WebView` 0.1.0 cannot cancel an in-flight load
once started. The only control points are up front, via `WebView::load` and
`WebViewHandler::intercept`.

## Rendering path

`WebView::show` reads Servo's offscreen framebuffer back to the CPU
(`read_to_image` → `egui::ColorImage` → a texture uploaded via
`ui.ctx().load_texture`), reusing one `TextureHandle` for the life of the view
rather than allocating a fresh one every frame. The read is gated on a
`frame_dirty` flag set by Servo's `notify_new_frame_ready` hook, so a
`show()` with nothing new to paint costs a texture upload, not a full-surface
GPU→CPU copy.

A true zero-copy path (`OffscreenRenderingContext::render_to_parent_callback`
blitting straight into egui's GL context via an `egui::PaintCallback`) was
attempted and reverted -- see PLAN.md's A6 section for the full account. In
short: it compiled, but the screenshot workflow this repo's `HANDOFF.md` §2
insists on came back blank, because `WebViewHost` builds Servo its own,
unshared GL context on the window handle rather than reusing the one
`eframe`/`glutin` already created there, so blitting into "the parent's own
framebuffer" never reaches what `egui_glow` is actually compositing to the
screen. Making that path work for real needs GL context sharing set up at
context-creation time, on both the Servo and `eframe` sides -- left for
whoever picks it up next.

## Setup

This crate needs the vendored `servo` crate to build, which is a large, slow
compile the first time (see this repo's `HANDOFF.md` §1 for real timings).

At runtime, on Windows, `libEGL.dll` and `libGLESv2.dll` must sit next to the
built executable (or be on `PATH`) -- Cargo does not copy them there. See
`HANDOFF.md` §3.9 for where to find them in this repo and why they're needed.

## What's deliberately not here

Per PLAN.md's A7 section: no `cargo doc` CI job, no polished
`examples/minimal.rs` browser, no crates.io metadata (license, publishable
semver surface) -- the mail client is the demo, and this crate is not going
anywhere outside this workspace.
