use dpi::PhysicalSize;
use ebrowser::ServoWebView;
use eframe::CreationContext;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

struct EbrowserApp {
    servo_view: ServoWebView,
}

impl EbrowserApp {
    fn new(cc: &CreationContext<'_>) -> Self {
        // Initialize logging
        let _ = env_logger::try_init();

        // As a demo simplification, let's use raw_window_handle.
        let display_handle = cc.display_handle().expect("Failed to get display handle").as_raw();
        let window_handle = cc.window_handle().expect("Failed to get window handle").as_raw();

        let servo_view = ServoWebView::new(cc.egui_ctx.clone(), display_handle, window_handle);

        Self { servo_view }
    }
}

impl eframe::App for EbrowserApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("ebrowser - Servo + egui");

            let available_size = ui.available_size();
            ui.label(format!("Rendering at {}x{}", available_size.x, available_size.y));

            let (_, urls) = self.servo_view.ui(ui);

            for url in urls {
                println!("Link clicked! Requested navigation to: {}", url);
            }
        });
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
