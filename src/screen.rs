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
    pub fn start(ch: i64, max_w: u32, fps: i64, draw_cur: bool, tx: Sender<rmpv::Value>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let period = Duration::from_millis((1000 / fps.clamp(1, 60)) as u64);
        std::thread::spawn(move || {
            let monitor = xcap::Monitor::all()
                .ok()
                .and_then(|list| list.into_iter().next());
            let Some(monitor) = monitor else {
                return;
            };
            let mon_x = monitor.x();
            let mon_y = monitor.y();
            let mut report = std::time::Instant::now();
            while !flag.load(Ordering::Relaxed) {
                let t0 = std::time::Instant::now();
                if let Ok(shot) = monitor.capture_image() {
                    let t_cap = t0.elapsed();
                    let (w, h) = (shot.width(), shot.height());
                    let raw = shot.into_raw();
                    if let Some(img) = image::RgbaImage::from_raw(w, h, raw) {
                        let (sw, sh) = (w, h);
                        let t1 = std::time::Instant::now();
                        let mut img = downscale(img, max_w.max(320));
                        let t_scale = t1.elapsed();
                        let sx = img.width() as f64 / w as f64;
                        let sy = img.height() as f64 / h as f64;
                        if draw_cur {
                            if let Some(pos) = cursor_pos() {
                                let cx = ((pos.0 - mon_x) as f64 * sx).round() as i32;
                                let cy = ((pos.1 - mon_y) as f64 * sy).round() as i32;
                                let size = (24.0 * sx.min(sy)).max(9.0);
                                draw_cursor(&mut img, cx, cy, size as i32);
                            }
                        }
                        let (fw, fh) = (img.width(), img.height());
                        let t2 = std::time::Instant::now();
                        let jpeg = encode_jpeg(img);
                        let t_enc = t2.elapsed();
                        if report.elapsed() >= Duration::from_secs(2) {
                            report = std::time::Instant::now();
                            eprintln!(
                                "[screen] cap {:?}  scale {:?}  enc {:?}  jpeg {}KB  {}x{}",
                                t_cap, t_scale, t_enc, jpeg.len() / 1024, fw, fh
                            );
                        }
                        // Never queue frames: if the writer/link is behind, drop
                        // this one so the client always gets the freshest, not a
                        // backlog (that backlog is the 0.3-0.5s of lag).
                        match tx.try_send(screen_frame(ch, fw, fh, sw, sh, jpeg)) {
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                            _ => {}
                        }
                    }
                }
                // Pace to the target fps by the time left after capture+encode.
                if let Some(rest) = period.checked_sub(t0.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
        });
        ScreenStream { stop }
    }

    #[cfg(not(feature = "screen"))]
    pub fn start(_ch: i64, _max_w: u32, _fps: i64, _draw_cur: bool, _tx: Sender<rmpv::Value>) -> Self {
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
    use fast_image_resize::images::Image;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    let (w, h) = (img.width(), img.height());
    let dw = max_w;
    let dh = ((h as u64 * dw as u64 / w as u64) as u32).max(1);

    let src = match Image::from_vec_u8(w, h, img.into_raw(), PixelType::U8x4) {
        Ok(s) => s,
        Err(_) => return image::RgbaImage::new(dw, dh),
    };
    let mut dst = Image::new(dw, dh, PixelType::U8x4);
    let mut resizer = Resizer::new();
    let _ = resizer.resize(
        &src,
        &mut dst,
        &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear)),
    );
    image::RgbaImage::from_raw(dw, dh, dst.into_vec()).unwrap_or_else(|| image::RgbaImage::new(dw, dh))
}

#[cfg(all(feature = "screen", windows))]
fn cursor_pos() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}

#[cfg(all(feature = "screen", not(windows)))]
fn cursor_pos() -> Option<(i32, i32)> {
    None // cursor overlay is Windows-only for now
}

/// A small white arrow with a black outline, at (cx, cy) in image pixels.
#[cfg(feature = "screen")]
fn draw_cursor(img: &mut image::RgbaImage, cx: i32, cy: i32, c: i32) {
    let c = c.max(7) as f32;
    // phone-mouse's 7-point pointer, in local coords.
    let pts: [(f32, f32); 7] = [
        (0.0, 0.0),
        (0.0, 0.82 * c),
        (0.22 * c, 0.63 * c),
        (0.38 * c, 1.00 * c),
        (0.54 * c, 0.93 * c),
        (0.37 * c, 0.57 * c),
        (0.72 * c, 0.57 * c),
    ];
    let white = image::Rgba([255, 255, 255, 255]);
    let black = image::Rgba([0, 0, 0, 255]);
    let (w, h) = (img.width() as i32, img.height() as i32);
    let y0 = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor() as i32;
    let y1 = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil() as i32;
    // scanline fill (white)
    for sy in y0..=y1 {
        let yf = sy as f32 + 0.5;
        let mut xs: Vec<f32> = Vec::new();
        for i in 0..pts.len() {
            let (ax, ay) = pts[i];
            let (bx, by) = pts[(i + 1) % pts.len()];
            if (ay <= yf && by > yf) || (by <= yf && ay > yf) {
                xs.push(ax + (yf - ay) / (by - ay) * (bx - ax));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut k = 0;
        while k + 1 < xs.len() {
            let a = (cx as f32 + xs[k]).round() as i32;
            let b = (cx as f32 + xs[k + 1]).round() as i32;
            for px in a..=b {
                let py = cy + sy;
                if px >= 0 && px < w && py >= 0 && py < h {
                    img.put_pixel(px as u32, py as u32, white);
                }
            }
            k += 2;
        }
    }
    // outline (black)
    for i in 0..pts.len() {
        let (ax, ay) = pts[i];
        let (bx, by) = pts[(i + 1) % pts.len()];
        let steps = ((bx - ax).abs().max((by - ay).abs()) as i32).max(1);
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let px = (cx as f32 + ax + (bx - ax) * t).round() as i32;
            let py = (cy as f32 + ay + (by - ay) * t).round() as i32;
            if px >= 0 && px < w && py >= 0 && py < h {
                img.put_pixel(px as u32, py as u32, black);
            }
        }
    }
}

#[cfg(feature = "screen")]
fn encode_jpeg(img: image::RgbaImage) -> Vec<u8> {
    // SIMD encoder, straight from RGBA (skips the RGBA->RGB copy the image crate needs).
    let (w, h) = (img.width() as u16, img.height() as u16);
    let mut out = Vec::with_capacity(96 * 1024);
    let enc = jpeg_encoder::Encoder::new(&mut out, 50);
    let _ = enc.encode(&img.into_raw(), w, h, jpeg_encoder::ColorType::Rgba);
    out
}
