use dpi::PhysicalSize;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use servo::{
    OffscreenRenderingContext, RenderingContext, Servo, ServoBuilder, WebView, WebViewBuilder,
    WebViewDelegate, WindowRenderingContext,
};
use std::rc::Rc;
use url::Url;

struct EbrowserDelegate {
    ctx: egui::Context,
    navigation_sender: std::sync::mpsc::Sender<Url>,
}

impl WebViewDelegate for EbrowserDelegate {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.ctx.request_repaint();
    }

    fn request_navigation(
        &self,
        _webview: WebView,
        navigation_request: servo::embedder_traits::NavigationRequest,
    ) {
        let url = navigation_request.url.clone();
        let _ = self.navigation_sender.send(url);
        // By default, let's allow it so the browser actually navigates,
        // but since we want to expose this to the egui user,
        // they can intercept the event.
        navigation_request.allow();
    }
}

pub struct ServoWebView {
    pub servo: Servo,
    pub web_view: WebView,
    pub offscreen_context: Rc<OffscreenRenderingContext>,
    navigation_receiver: std::sync::mpsc::Receiver<Url>,
}

impl ServoWebView {
    pub fn new(
        ctx: egui::Context,
        display_handle: raw_window_handle::RawDisplayHandle,
        window_handle: raw_window_handle::RawWindowHandle,
    ) -> Self {
        // Create the Servo engine
        let servo = ServoBuilder::default().build();

        // Setup rendering context
        // We need a WindowRenderingContext to create an OffscreenRenderingContext
        let window_size = PhysicalSize::new(1280, 720);

        // A mock type to hold the raw handles to satisfy `HasDisplayHandle` and `HasWindowHandle`
        struct RawHandleWrapper {
            display: raw_window_handle::RawDisplayHandle,
            window: raw_window_handle::RawWindowHandle,
        }

        impl HasDisplayHandle for RawHandleWrapper {
            fn display_handle(
                &self,
            ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
                // Safety: We assume the caller provides valid handles
                Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(self.display) })
            }
        }

        impl HasWindowHandle for RawHandleWrapper {
            fn window_handle(
                &self,
            ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
                // Safety: We assume the caller provides valid handles
                Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(self.window) })
            }
        }

        let handles = RawHandleWrapper {
            display: display_handle,
            window: window_handle,
        };

        let window_context = WindowRenderingContext::new(
            handles.display_handle().expect("Failed to get display handle"),
            handles.window_handle().expect("Failed to get window handle"),
            window_size,
        )
        .expect("Failed to create WindowRenderingContext");
        let window_context = Rc::new(window_context);

        let initial_view_size = PhysicalSize::new(1024, 768);
        let offscreen_context = Rc::new(window_context.offscreen_context(initial_view_size));

        // Create WebView with a delegate
        let initial_url = Url::parse("https://servo.org").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();

        let delegate = Rc::new(EbrowserDelegate {
            ctx,
            navigation_sender: tx
        });

        let web_view =
            WebViewBuilder::new(&servo, offscreen_context.clone() as Rc<dyn RenderingContext>)
                .url(initial_url)
                .delegate(delegate)
                .build();

        Self {
            servo,
            web_view,
            offscreen_context,
            navigation_receiver: rx,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> (egui::Response, Vec<Url>) {
        let available_size = ui.available_size();
        let pixels_per_point = ui.ctx().pixels_per_point();

        self.web_view.set_hidpi_scale_factor(euclid::Scale::new(pixels_per_point));

        // Resize Servo if available_size changed significantly
        // The scale factor is needed to convert logical sizes to physical pixels
        let new_size = PhysicalSize::new(
            (available_size.x * pixels_per_point) as u32,
            (available_size.y * pixels_per_point) as u32
        );
        if new_size != self.offscreen_context.size() {
            self.web_view.resize(new_size);
        }

        // Allocate space for the WebView by placing the image.
        // We ensure Servo has finished its work for this frame
        self.web_view.paint();

        let image_rect = euclid::Box2D::<i32, servo::DevicePixel>::new(
            euclid::Point2D::new(0, 0),
            euclid::Point2D::new(new_size.width as i32, new_size.height as i32),
        );

        let mut drew_image = false;
        let mut response = None;

        if let Some(rgba) = self.offscreen_context.read_to_image(image_rect) {
            let size = [rgba.width() as usize, rgba.height() as usize];
            if size[0] > 0 && size[1] > 0 {
                let pixels = rgba.as_raw();
                let color_image = egui::ColorImage::from_rgba_unmultiplied(size, pixels);

                let texture = ui
                    .ctx()
                    .load_texture("servo_fbo", color_image, egui::TextureOptions::LINEAR);

                response = Some(ui.add(
                    egui::Image::new(&texture)
                        .fit_to_exact_size(available_size)
                        .sense(egui::Sense::click_and_drag()),
                ));
                drew_image = true;
            }
        }

        // Handle events
        if let Some(mut response_val) = response {
            response_val = response_val.interact(egui::Sense::click_and_drag());
            let pointer_state = ui.input(|i| i.pointer.clone());

            use servo::embedder_traits::{
                InputEvent, MouseButtonEvent, MouseButtonAction, MouseButton, MouseMoveEvent,
                WheelEvent, WheelDelta, WheelMode, WebViewPoint,
            };
            use euclid::Point2D;

            // Note: We need to scale egui coordinates to physical device coordinates
            let scale_factor = ui.ctx().pixels_per_point();

            if let Some(pos) = response_val.hover_pos() {
                let rect = response_val.rect;
                // Calculate position relative to the top-left of the webview
                let x = (pos.x - rect.min.x) * scale_factor;
                let y = (pos.y - rect.min.y) * scale_factor;

                let point = WebViewPoint::Page(Point2D::new(x as f32, y as f32));

                // Mouse Move
                if response_val.hovered() && pointer_state.velocity().length_sq() > 0.0 {
                    self.web_view.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point.clone())));
                }

                // Mouse Buttons
                let egui_buttons = [
                    (egui::PointerButton::Primary, MouseButton::Left),
                    (egui::PointerButton::Secondary, MouseButton::Right),
                    (egui::PointerButton::Middle, MouseButton::Middle),
                ];

                for (egui_btn, servo_btn) in egui_buttons {
                    if response_val.clicked_by(egui_btn) || response_val.drag_started_by(egui_btn) {
                        self.web_view.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                            MouseButtonAction::Down,
                            servo_btn.clone(),
                            point.clone(),
                        )));
                    }
                    if response_val.drag_stopped() {
                        self.web_view.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                            MouseButtonAction::Up,
                            servo_btn.clone(),
                            point.clone(),
                        )));
                    }
                }

                // Scroll/Wheel
                let scroll_delta = ui.input(|i| i.smooth_scroll_delta);
                if scroll_delta != egui::Vec2::ZERO {
                    self.web_view.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                        WheelDelta {
                            x: -scroll_delta.x as f64 * scale_factor as f64,
                            y: -scroll_delta.y as f64 * scale_factor as f64,
                            z: 0.0,
                            mode: WheelMode::DeltaPixel,
                        },
                        point,
                    )));
                }
            }

            self.servo.spin_event_loop();
            let urls: Vec<Url> = self.navigation_receiver.try_iter().collect();
            (response_val, urls)
        } else {
            self.servo.spin_event_loop();
            let urls: Vec<Url> = self.navigation_receiver.try_iter().collect();
            (ui.allocate_exact_size(available_size, egui::Sense::hover()).1, urls)
    }
}
