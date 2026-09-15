//! Two independent [`WebView`]s sharing one [`WebViewHost`], side by side.
//!
//! Ported from the Servo-backed predecessor crate's own `two_views` example,
//! which proved "one engine, N views" held for Servo -- this proves the
//! litehtml-backed replacement holds the same shape: two independently
//! scrolling, independently loaded views from one host, with no engine
//! driving loop needed at all now (litehtml's `pixbuf` backend has no event
//! loop to spin; `WebViewHost::spin()` no longer exists).
//!
//! Run with:
//! ```text
//! cargo run --example two_views -p egui-litehtml-webview
//! ```

use egui_litehtml_webview::{WebView, WebViewConfig, WebViewHost, WebViewSource};

struct TwoViewsApp {
    // `host` outlives both views only by convention here (litehtml's
    // pixbuf backend owns no shared engine state the views actually
    // depend on at drop time, unlike Servo) -- kept anyway so this example
    // still demonstrates the intended one-host-many-views call shape.
    host: WebViewHost,
    left: WebView,
    right: WebView,
}

impl TwoViewsApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let host = WebViewHost::new();

        let left = host.new_view(
            &cc.egui_ctx,
            WebViewConfig::new(WebViewSource::Html(left_page())),
        );
        let right = host.new_view(
            &cc.egui_ctx,
            WebViewConfig::new(WebViewSource::Html(right_page())),
        );

        Self { host, left, right }
    }
}

impl eframe::App for TwoViewsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let _ = &self.host; // no per-frame driving needed; see the struct doc.

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.columns(2, |columns| {
                columns[0].heading("Left view");
                for event in self.left.show(&mut columns[0]) {
                    log::debug!("left: {event:?}");
                }

                columns[1].heading("Right view");
                for event in self.right.show(&mut columns[1]) {
                    log::debug!("right: {event:?}");
                }
            });
        });
    }
}

fn left_page() -> String {
    r#"<!doctype html>
<meta charset="utf-8">
<style>body { font: 16px system-ui, sans-serif; background: #eef6ff; margin: 1rem; }</style>
<h1>I am the left view</h1>
<p>Scroll me, click my own link -- none of it should touch the view on the
right.</p>
<p><a href="https://example.com/left">A link only this view knows about</a></p>
<div style="height: 60vh; background: linear-gradient(#dbe9ff, #fff);"></div>
<p>Bottom of the left page.</p>
"#
    .to_string()
}

fn right_page() -> String {
    r#"<!doctype html>
<meta charset="utf-8">
<style>body { font: 16px system-ui, sans-serif; background: #fff4ea; margin: 1rem; }</style>
<h1>I am the right view</h1>
<p>A different page, a different scroll position -- proof that two
<code>WebView</code>s from one <code>WebViewHost</code> are genuinely
independent.</p>
<div style="height: 60vh; background: linear-gradient(#ffe7cf, #fff);"></div>
<p>Bottom of the right page.</p>
"#
    .to_string()
}

fn main() -> eframe::Result {
    env_logger::init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "egui-litehtml-webview: two_views",
        native_options,
        Box::new(|cc| Ok(Box::new(TwoViewsApp::new(cc)))),
    )
}
