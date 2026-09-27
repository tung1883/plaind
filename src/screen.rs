//! The `screen` channel — a low-fi JPEG mirror of the primary monitor.
//!
//! Each tick: capture (skipped entirely when the desktop hasn't changed),
//! downscale, diff against the last frame the client got in 64 px tiles, and
//! send either just the changed tiles or — first frame, or most of the screen
//! changed — one full frame. Clients that ack are paced by a byte budget that
//! grows while round trips stay near the link's floor and shrinks as soon as
//! they inflate, so frames never queue up in the network.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;

/// A lost ack must not freeze the stream: after this long, send anyway.
const ACK_STALL: Duration = Duration::from_secs(2);
/// Byte budget bounds for un-acked frames in flight.
const BUDGET_MIN: usize = 64 * 1024;
const BUDGET_MAX: usize = 4 * 1024 * 1024;
/// Tile edge for change detection, in delivered-frame pixels (a multiple of
/// 16 so tile edges line up with JPEG's 4:2:0 blocks).
#[cfg(feature = "screen")]
const TILE: u32 = 64;
/// Send a full frame instead of tiles once this share of tiles changed.
#[cfg(feature = "screen")]
const FULL_SHARE: f64 = 0.6;

/// Ack-driven flow control, shared by the capture thread and `ack()`.
struct Flow {
    st: Mutex<FlowState>,
    cv: Condvar,
}

struct FlowState {
    /// (bytes, sent at) per un-acked frame, oldest first.
    inflight: VecDeque<(usize, Instant)>,
    bytes: usize,
    budget: usize,
    /// Lowest round trip seen lately — the link's floor without queueing.
    min_rtt: Duration,
    min_rtt_at: Instant,
}

impl Flow {
    fn new() -> Self {
        Flow {
            st: Mutex::new(FlowState {
                inflight: VecDeque::new(),
                bytes: 0,
                budget: 256 * 1024,
                min_rtt: Duration::MAX,
                min_rtt_at: Instant::now(),
            }),
            cv: Condvar::new(),
        }
    }

    /// Block until another frame may go: nothing in flight, or in-flight bytes
    /// under budget. After `ACK_STALL` without progress, assume the acks were
    /// lost and start over.
    fn wait_ready(&self, stop: &AtomicBool) {
        let t0 = Instant::now();
        let mut st = self.st.lock().unwrap();
        while !(st.inflight.is_empty() || st.bytes < st.budget) {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if t0.elapsed() >= ACK_STALL {
                st.inflight.clear();
                st.bytes = 0;
                break;
            }
            st = self.cv.wait_timeout(st, Duration::from_millis(50)).unwrap().0;
        }
        crate::latstat::record("d.screen.flow_wait", crate::latstat::ms(t0.elapsed()));
    }

    fn sent(&self, bytes: usize) {
        let mut st = self.st.lock().unwrap();
        st.inflight.push_back((bytes, Instant::now()));
        st.bytes += bytes;
    }

    fn ack(&self) {
        let mut st = self.st.lock().unwrap();
        let Some((bytes, at)) = st.inflight.pop_front() else {
            return;
        };
        st.bytes -= bytes;
        let rtt = at.elapsed();
        // Re-learn the floor every 10 s so a route change can raise it.
        if rtt < st.min_rtt || st.min_rtt_at.elapsed() > Duration::from_secs(10) {
            st.min_rtt = rtt;
            st.min_rtt_at = Instant::now();
        }
        // Delay-based AIMD: round trips near the floor mean the link has room;
        // inflated ones mean our own frames are queueing somewhere en route.
        let target = st.min_rtt + st.min_rtt / 4 + Duration::from_millis(15);
        st.budget = if rtt <= target {
            (st.budget + 32 * 1024).min(BUDGET_MAX)
        } else {
            (st.budget * 7 / 10).max(BUDGET_MIN)
        };
        crate::latstat::record("d.screen.rtt", crate::latstat::ms(rtt));
        crate::latstat::record("d.screen.budget_kb", st.budget as f64 / 1024.0);
        self.cv.notify_one();
    }
}

pub struct ScreenStream {
    stop: Arc<AtomicBool>,
    flow: Option<Arc<Flow>>,
}

impl ScreenStream {
    /// The client received a frame — let the next one go.
    pub fn ack(&self) {
        if let Some(f) = &self.flow {
            f.ack();
        }
    }

    #[cfg(feature = "screen")]
    pub fn start(
        ch: i64,
        max_w: u32,
        fps: i64,
        draw_cur: bool,
        ack: bool,
        tiles: bool,
        tx: Sender<rmpv::Value>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let flow = ack.then(|| Arc::new(Flow::new()));
        let gate = flow.clone();
        let period = Duration::from_millis((1000 / fps.clamp(1, 60)) as u64);
        std::thread::spawn(move || {
            let Some(mut cap) = crate::capture::open() else {
                return;
            };
            // The last frame the client has (delivered size, pre-JPEG pixels).
            let mut shown: Option<(u32, u32, Vec<u8>)> = None;
            let mut next_tick = Instant::now();
            let mut report = Instant::now();
            while !flag.load(Ordering::Relaxed) {
                if let Some(f) = &gate {
                    f.wait_ready(&flag);
                }
                let now = Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                }
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                let t0 = Instant::now();
                let shot = match cap.next(Duration::from_millis(100)) {
                    Ok(Some(s)) => s,
                    Ok(None) => continue, // desktop unchanged
                    Err(e) => {
                        crate::plog!("[screen] capture failed: {e}");
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                };
                next_tick = t0 + period;
                let t_cap = t0.elapsed();
                let (sw, sh, bgra) = (shot.width, shot.height, shot.bgra);

                let t1 = Instant::now();
                let (fw, fh, mut px) = downscale(shot.pixels, sw, sh, max_w.max(320));
                if draw_cur {
                    if let Some(pos) = cursor_pos() {
                        let (ox, oy) = cap.origin();
                        let sx = fw as f64 / sw as f64;
                        let sy = fh as f64 / sh as f64;
                        let cx = ((pos.0 - ox) as f64 * sx).round() as i32;
                        let cy = ((pos.1 - oy) as f64 * sy).round() as i32;
                        let size = (24.0 * sx.min(sy)).max(9.0);
                        draw_cursor(&mut px, fw, fh, cx, cy, size as i32);
                    }
                }
                let t_scale = t1.elapsed();

                let t2 = Instant::now();
                let prev = shown.as_ref().filter(|(w, h, _)| tiles && *w == fw && *h == fh);
                let msg = match prev {
                    Some((_, _, old)) => {
                        let (rects, changed, total) = changed_rects(old, &px, fw, fh);
                        if changed == 0 {
                            continue; // e.g. only the cursor or an off-frame pixel moved
                        }
                        if changed as f64 / total as f64 >= FULL_SHARE {
                            None
                        } else {
                            let parts: Vec<(u32, u32, u32, u32, Vec<u8>)> = rects
                                .into_iter()
                                .map(|(x, y, w, h)| {
                                    let sub = crop(&px, fw, x, y, w, h);
                                    (x, y, w, h, encode_jpeg(&sub, w, h, bgra))
                                })
                                .collect();
                            Some(parts)
                        }
                    }
                    None => None,
                };
                let (msg, bytes, kind) = match msg {
                    Some(parts) => {
                        let bytes: usize = parts.iter().map(|p| p.4.len()).sum();
                        crate::latstat::record("d.screen.tiles", parts.len() as f64);
                        (crate::proto::screen_tiles(ch, fw, fh, sw, sh, parts), bytes, "tiles")
                    }
                    None => {
                        let jpeg = encode_jpeg(&px, fw, fh, bgra);
                        let bytes = jpeg.len();
                        (crate::proto::screen_frame(ch, fw, fh, sw, sh, jpeg), bytes, "full")
                    }
                };
                let t_enc = t2.elapsed();
                crate::latstat::record("d.screen.capture", crate::latstat::ms(t_cap));
                crate::latstat::record("d.screen.scale", crate::latstat::ms(t_scale));
                crate::latstat::record("d.screen.encode", crate::latstat::ms(t_enc));
                crate::latstat::record(
                    if kind == "full" { "d.screen.full_kb" } else { "d.screen.tiles_kb" },
                    bytes as f64 / 1024.0,
                );
                if report.elapsed() >= Duration::from_secs(2) {
                    report = Instant::now();
                    crate::plog!(
                        "[screen] cap {:?}  scale {:?}  enc {:?}  {kind} {}KB  {}x{}",
                        t_cap, t_scale, t_enc, bytes / 1024, fw, fh
                    );
                }
                // Never queue frames: if the writer is behind, drop this one.
                // `shown` only advances on success, so the next diff still
                // carries whatever this dropped update would have.
                match tx.try_send(msg) {
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                    Ok(()) => {
                        if let Some(f) = &gate {
                            f.sent(bytes);
                        }
                        shown = Some((fw, fh, px));
                    }
                }
            }
        });
        ScreenStream { stop, flow }
    }

    #[cfg(not(feature = "screen"))]
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        _ch: i64,
        _max_w: u32,
        _fps: i64,
        _draw_cur: bool,
        _ack: bool,
        _tiles: bool,
        _tx: Sender<rmpv::Value>,
    ) -> Self {
        ScreenStream {
            stop: Arc::new(AtomicBool::new(true)),
            flow: None,
        }
    }

    pub const SUPPORTED: bool = cfg!(feature = "screen");
}

impl Drop for ScreenStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(f) = &self.flow {
            f.cv.notify_one();
        }
    }
}

/// Changed tiles between two same-size frames, merged into horizontal runs per
/// tile row: `(rects as x,y,w,h, changed tiles, total tiles)`.
#[cfg(feature = "screen")]
fn changed_rects(old: &[u8], new: &[u8], w: u32, h: u32) -> (Vec<(u32, u32, u32, u32)>, usize, usize) {
    let (tx, ty) = (w.div_ceil(TILE), h.div_ceil(TILE));
    let stride = w as usize * 4;
    let mut rects = Vec::new();
    let mut changed = 0;
    for j in 0..ty {
        let y0 = j * TILE;
        let th = TILE.min(h - y0);
        let mut run: Option<u32> = None; // first tile column of the current run
        for i in 0..=tx {
            let dirty = i < tx && {
                let x0 = (i * TILE) as usize * 4;
                let tw = TILE.min(w - i * TILE) as usize * 4;
                (y0..y0 + th).any(|y| {
                    let o = y as usize * stride + x0;
                    old[o..o + tw] != new[o..o + tw]
                })
            };
            if dirty {
                changed += 1;
                run.get_or_insert(i);
            } else if let Some(start) = run.take() {
                let x = start * TILE;
                let rw = (i * TILE).min(w) - x;
                rects.push((x, y0, rw, th));
            }
        }
    }
    (rects, changed, (tx * ty) as usize)
}

#[cfg(feature = "screen")]
fn crop(px: &[u8], w: u32, x: u32, y: u32, cw: u32, ch: u32) -> Vec<u8> {
    let stride = w as usize * 4;
    let row = cw as usize * 4;
    let mut out = Vec::with_capacity(row * ch as usize);
    for yy in y..y + ch {
        let o = yy as usize * stride + x as usize * 4;
        out.extend_from_slice(&px[o..o + row]);
    }
    out
}

#[cfg(feature = "screen")]
/// Shrink 4-byte pixels to at most `max_w` wide (channel order untouched).
fn downscale(px: Vec<u8>, w: u32, h: u32, max_w: u32) -> (u32, u32, Vec<u8>) {
    if w <= max_w {
        return (w, h, px);
    }
    use fast_image_resize::images::Image;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    let dw = max_w;
    let dh = ((h as u64 * dw as u64 / w as u64) as u32).max(1);

    let src = match Image::from_vec_u8(w, h, px, PixelType::U8x4) {
        Ok(s) => s,
        Err(_) => return (dw, dh, vec![0; dw as usize * dh as usize * 4]),
    };
    let mut dst = Image::new(dw, dh, PixelType::U8x4);
    let mut resizer = Resizer::new();
    let _ = resizer.resize(
        &src,
        &mut dst,
        &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear)),
    );
    (dw, dh, dst.into_vec())
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
fn draw_cursor(px: &mut [u8], w: u32, h: u32, cx: i32, cy: i32, c: i32) {
    // white / black are the same in RGBA and BGRA order
    let mut put = |x: i32, y: i32, v: u8| {
        let o = (y as usize * w as usize + x as usize) * 4;
        px[o..o + 4].copy_from_slice(&[v, v, v, 255]);
    };
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
    let (w, h) = (w as i32, h as i32);
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
                    put(px, py, 255);
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
                put(px, py, 0);
            }
        }
    }
}

#[cfg(feature = "screen")]
fn encode_jpeg(px: &[u8], w: u32, h: u32, bgra: bool) -> Vec<u8> {
    // SIMD encoder, straight from 4-byte pixels (no RGB repack).
    let mut out = Vec::with_capacity(px.len() / 16);
    let enc = jpeg_encoder::Encoder::new(&mut out, 50);
    let color = if bgra { jpeg_encoder::ColorType::Bgra } else { jpeg_encoder::ColorType::Rgba };
    let _ = enc.encode(px, w as u16, h as u16, color);
    out
}

#[cfg(all(test, feature = "screen"))]
mod tests {
    use super::*;

    #[test]
    fn changed_rects_merges_runs_per_tile_row() {
        let (w, h) = (200u32, 130u32); // 4x3 tiles, ragged right and bottom edges
        let old = vec![0u8; (w * h * 4) as usize];
        let mut new = old.clone();
        let mut poke = |x: u32, y: u32| new[((y * w + x) * 4) as usize] = 1;
        poke(10, 10); // tile (0,0)
        poke(70, 20); // tile (1,0) — joins (0,0) into one run
        poke(199, 129); // tile (3,2), the ragged corner
        let (rects, changed, total) = changed_rects(&old, &new, w, h);
        assert_eq!(total, 12);
        assert_eq!(changed, 3);
        assert_eq!(rects, vec![(0, 0, 128, 64), (192, 128, 8, 2)]);
    }

    #[test]
    fn unchanged_frame_has_no_rects() {
        let px = vec![7u8; 64 * 64 * 4];
        assert_eq!(changed_rects(&px, &px, 64, 64), (vec![], 0, 1));
    }
}
