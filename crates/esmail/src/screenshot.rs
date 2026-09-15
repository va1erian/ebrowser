//! Dump what the app actually looks like to a PNG, so a change can be checked
//! visually without a human watching the window.
//!
//! Two triggers:
//!
//! * **F12** — writes `esmail-screenshot-<n>.png` into the current directory.
//! * **`ESMAIL_SCREENSHOT=<path>`** — captures automatically once the UI has
//!   settled and then exits. `ESMAIL_SCREENSHOT_FRAMES` (default 30) controls
//!   how many frames to let pass first.
//!
//! The capture goes through egui's own `ViewportCommand::Screenshot`, so it is
//! the composited window — egui chrome *and* the webview's texture — not
//! just the page.

use std::path::PathBuf;

pub struct Screenshotter {
    /// Set from ESMAIL_SCREENSHOT; capture then exit.
    auto_path: Option<PathBuf>,
    /// Frames to wait before the automatic capture.
    auto_after_frames: u32,
    frames_seen: u32,
    /// A capture has been requested and we are waiting for the reply event.
    pending: bool,
    /// Number of manual (F12) captures so far, used to name the files.
    manual_count: u32,
}

impl Screenshotter {
    pub fn from_env() -> Self {
        let auto_path = std::env::var_os("ESMAIL_SCREENSHOT").map(PathBuf::from);
        let auto_after_frames = std::env::var("ESMAIL_SCREENSHOT_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);

        Self {
            auto_path,
            auto_after_frames,
            frames_seen: 0,
            pending: false,
            manual_count: 0,
        }
    }

    /// True when the app was started purely to produce a screenshot.
    pub fn is_automatic(&self) -> bool {
        self.auto_path.is_some()
    }

    /// Call once per frame. Requests captures and writes out any that arrived.
    pub fn update(&mut self, ctx: &egui::Context) {
        self.frames_seen += 1;

        // Keep frames coming even when nothing else is animating, or an
        // automatic capture would wait forever for an idle UI to repaint.
        if self.is_automatic() {
            ctx.request_repaint();
        }

        let manual = ctx.input(|i| i.key_pressed(egui::Key::F12));
        let automatic = self.auto_path.is_some() && self.frames_seen >= self.auto_after_frames;

        if !self.pending && (manual || automatic) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            self.pending = true;
        }

        let captured = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });

        let Some(image) = captured else { return };
        self.pending = false;

        let path = match self.auto_path.clone() {
            Some(path) => path,
            None => {
                self.manual_count += 1;
                PathBuf::from(format!("esmail-screenshot-{}.png", self.manual_count))
            }
        };

        match write_png(&path, &image) {
            Ok(()) => log::info!("wrote screenshot to {}", path.display()),
            Err(e) => log::error!("could not write screenshot to {}: {e}", path.display()),
        }

        if self.is_automatic() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn write_png(path: &std::path::Path, image: &egui::ColorImage) -> anyhow::Result<()> {
    let [w, h] = image.size;
    // egui stores premultiplied RGBA bytes. A window screenshot is opaque, so
    // writing them straight out is correct here.
    let buffer = image::RgbaImage::from_raw(w as u32, h as u32, image.as_raw().to_vec())
        .ok_or_else(|| anyhow::anyhow!("screenshot buffer was {w}x{h} but had the wrong length"))?;
    buffer.save(path)?;
    Ok(())
}
