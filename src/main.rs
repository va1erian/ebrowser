//! Demo application for the `es_webview` library.
//!
//! Launches an egui window and renders https://servo.org (or any other
//! `WebViewSource`) using the reusable `ESWebView` component.

use es_webview::{ESWebView, ESWebViewEvent, WebViewSource};

struct EbrowserApp {
    web_view: ESWebView,
    event_log: Vec<String>,
}

impl EbrowserApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let _ = env_logger::try_init();
        let source = WebViewSource::Url("https://servo.org".to_string());
        Self {
            web_view: ESWebView::new(cc, source),
            event_log: Vec::new(),
        }
    }
}

impl eframe::App for EbrowserApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Top bar
        ui.horizontal(|ui| {
            ui.heading("ebrowser – powered by es_webview");
            if !self.event_log.is_empty() {
                ui.separator();
                ui.label(format!(
                    "Last event: {}",
                    self.event_log.last().unwrap()
                ));
            }
        });
        ui.separator();

        // Reserve the rest of the panel for the webview.
        let events = self.web_view.show(ui);

        // Handle events (link clicks etc.).
        for evt in events {
            match &evt {
                ESWebViewEvent::LinkClicked(url) => {
                    log::info!("[ESWebView] Link clicked: {url}");
                    self.event_log.push(format!("LinkClicked({url})"));
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "ebrowser",
        native_options,
        Box::new(|cc| Ok(Box::new(EbrowserApp::new(cc)))),
    )
}
