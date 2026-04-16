
use servo::{
    Servo, ServoBuilder, WebViewBuilder, WebView, 
    OffscreenRenderingContext, WindowRenderingContext,
    WebViewDelegate, RenderingContext,
};
use dpi::PhysicalSize;
use raw_window_handle::{HasWindowHandle, HasDisplayHandle};

use std::rc::Rc;
use url::Url;

struct EbrowserDelegate {
    ctx: egui::Context,
}

impl WebViewDelegate for EbrowserDelegate {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.ctx.request_repaint();
    }
}

struct EbrowserApp {
    servo: Servo,
    web_view: WebView,
    offscreen_context: Rc<OffscreenRenderingContext>,
}

impl EbrowserApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Initialize logging
        let _ = env_logger::try_init();

        // Create the Servo engine
        let servo = ServoBuilder::default().build();
        
        // Setup rendering context
        // We need a WindowRenderingContext to create an OffscreenRenderingContext
        let window_size = PhysicalSize::new(1280, 720);
        
        let display_handle = cc.display_handle().expect("Failed to get display handle");
        let window_handle = cc.window_handle().expect("Failed to get window handle");

        let window_context = WindowRenderingContext::new(
            display_handle,
            window_handle,
            window_size,
        ).expect("Failed to create WindowRenderingContext");
        let window_context = Rc::new(window_context);
        
        let initial_view_size = PhysicalSize::new(1024, 768);
        let offscreen_context = Rc::new(window_context.offscreen_context(initial_view_size));
        
        // Create WebView with a delegate
        let initial_url = Url::parse("https://servo.org").unwrap();
        let delegate = Rc::new(EbrowserDelegate {
            ctx: cc.egui_ctx.clone(),
        });

        let web_view = WebViewBuilder::new(&servo, offscreen_context.clone() as Rc<dyn RenderingContext>)
            .url(initial_url)
            .delegate(delegate)
            .build();

        Self {
            servo,
            web_view,
            offscreen_context,
        }
    }
}

impl eframe::App for EbrowserApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        
        ui.heading("ebrowser - Servo + egui");
        
        let available_size = ui.available_size();
        
        // Resize Servo if available_size changed significantly
        let new_size = PhysicalSize::new(available_size.x as u32, available_size.y as u32);
        if new_size != self.offscreen_context.size() {
            self.web_view.resize(new_size);
        }
        
        ui.label(format!("Rendering at {}x{}", available_size.x, available_size.y));
        
        // Allocate space for the WebView by placing the image.
        // We ensure Servo has finished its work for this frame
        self.web_view.paint();

        let image_rect = euclid::Box2D::<i32, servo::DevicePixel>::new(
            euclid::Point2D::new(0, 0),
            euclid::Point2D::new(new_size.width as i32, new_size.height as i32),
        );
        
        let mut drew_image = false;
        if let Some(rgba) = self.offscreen_context.read_to_image(image_rect) {
            let size = [rgba.width() as usize, rgba.height() as usize];
            if size[0] > 0 && size[1] > 0 {
                let pixels = rgba.as_raw();
                let color_image = egui::ColorImage::from_rgba_unmultiplied(size, pixels);
                
                let texture = ui.ctx().load_texture(
                    "servo_fbo", 
                    color_image, 
                    egui::TextureOptions::LINEAR
                );
                
                ui.add(
                    egui::Image::new(&texture)
                        .fit_to_exact_size(available_size)
                        .sense(egui::Sense::click_and_drag())
                );
                drew_image = true;
            }
        }

        if !drew_image {
            ui.allocate_exact_size(available_size, egui::Sense::hover());
        }

        // Handle events
        self.servo.spin_event_loop();
        
        // Request a repaint to keep the event loop alive
        ctx.request_repaint();
    }
}



#[tokio::main]
async fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };
    
    eframe::run_native(
        "ebrowser",
        native_options,
        Box::new(|cc| Ok(Box::new(EbrowserApp::new(cc)))),
    )
}
