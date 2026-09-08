//! The `screen` channel — a low-fi JPEG mirror of the primary monitor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;

use crate::proto::screen_frame;

pub struct ScreenStream {
    stop: Arc<AtomicBool>,
}

impl ScreenStream {
    #[cfg(feature = "screen")]
    pub fn start(ch: i64, max_w: u32, fps: i64, tx: Sender<rmpv::Value>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let period = Duration::from_millis((1000 / fps.clamp(1, 30)) as u64);
        std::thread::spawn(move || {
            let monitor = xcap::Monitor::all()
                .ok()
                .and_then(|list| list.into_iter().next());
            let Some(monitor) = monitor else {
                return;
            };
            while !flag.load(Ordering::Relaxed) {
                if let Ok(shot) = monitor.capture_image() {
                    let (w, h) = (shot.width(), shot.height());
                    let raw = shot.into_raw();
                    if let Some(img) = image::RgbaImage::from_raw(w, h, raw) {
                        let img = downscale(img, max_w.max(320));
                        let (fw, fh) = (img.width(), img.height());
                        let jpeg = encode_jpeg(img);
                        if tx.blocking_send(screen_frame(ch, fw, fh, jpeg)).is_err() {
                            break;
                        }
                    }
                }
                std::thread::sleep(period);
            }
        });
        ScreenStream { stop }
    }

    #[cfg(not(feature = "screen"))]
    pub fn start(_ch: i64, _max_w: u32, _fps: i64, _tx: Sender<rmpv::Value>) -> Self {
        ScreenStream {
            stop: Arc::new(AtomicBool::new(true)),
        }
    }

    pub const SUPPORTED: bool = cfg!(feature = "screen");
}

impl Drop for ScreenStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(feature = "screen")]
fn downscale(img: image::RgbaImage, max_w: u32) -> image::RgbaImage {
    if img.width() <= max_w {
        return img;
    }
    let h = (img.height() as u64 * max_w as u64 / img.width() as u64) as u32;
    image::imageops::resize(&img, max_w, h.max(1), image::imageops::FilterType::Triangle)
}

#[cfg(feature = "screen")]
fn encode_jpeg(img: image::RgbaImage) -> Vec<u8> {
    let rgb = image::DynamicImage::ImageRgba8(img).into_rgb8();
    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 55);
    let _ = encoder.encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8);
    out
}
