//! Two independent [`WebView`]s sharing one [`WebViewHost`], side by side.
//!
//! This is deliberately not a polished browser (PLAN.md's A7 section explicitly
//! scopes that out -- the mail client is the demo for this crate). Its only
//! job is to be the real proof that A2's "one engine, N views" split actually
//! holds: two views, two different pages, each scrolling and taking input
//! independently, both driven by a single `WebViewHost::spin()` per frame.
//!
//! Run with:
//! ```text
//! cargo run --example two_views -p egui-servo-webview
//! ```

use dpi::PhysicalSize;
use egui_servo_webview::{WebView, WebViewConfig, WebViewHost, WebViewSource};

struct TwoViewsApp {
    host: WebViewHost,
    left: WebView,
    right: WebView,
}

impl TwoViewsApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let host = WebViewHost::from_eframe(cc, PhysicalSize::new(1280, 720))
            .expect("failed to create WebViewHost");

        // Two views from the one host -- each gets its own offscreen
        // rendering context and its own navigation/scroll/input state, but
        // both are backed by the same Servo engine underneath.
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
        // One engine spin per frame, however many views exist.
        self.host.spin();

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
<p>Scroll me, type in me, click my own link -- none of it should touch the
view on the right.</p>
<p><a href="https://example.com/left">A link only this view knows about</a></p>
<input type="text" placeholder="type here">
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
<p>A different page, a different scroll position, a different focused
input -- proof that two <code>WebView</code>s from one
<code>WebViewHost</code> are genuinely independent.</p>
<input type="text" placeholder="type here too">
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
        "egui-servo-webview: two_views",
        native_options,
        Box::new(|cc| Ok(Box::new(TwoViewsApp::new(cc)))),
    )
}
