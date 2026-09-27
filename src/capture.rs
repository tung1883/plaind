//! Primary-monitor capture. On Windows this is DXGI Desktop Duplication: a
//! GPU copy of the desktop in a few ms that also says when nothing changed,
//! so an idle screen costs no capture, encode or bandwidth at all. Anything
//! else (or a Windows box where duplication can't start) falls back to
//! `xcap`, which grabs a full frame every time.

#![cfg(feature = "screen")]

use std::time::Duration;

/// One captured desktop image, 4 bytes per pixel, tightly packed.
pub struct Captured {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// Channel order is B,G,R,A (DXGI) rather than R,G,B,A (xcap).
    pub bgra: bool,
}

pub trait Capturer {
    /// The next desktop image, or `None` if nothing changed within `timeout`.
    fn next(&mut self, timeout: Duration) -> anyhow::Result<Option<Captured>>;
    /// Top-left of the captured monitor in virtual-desktop coordinates.
    fn origin(&self) -> (i32, i32);
}

/// Opened on the capture thread itself — the handles stay on that thread.
pub fn open() -> Option<Box<dyn Capturer>> {
    #[cfg(windows)]
    {
        match dxgi::Dxgi::new() {
            Ok(d) => {
                crate::plog!("[screen] capture: DXGI desktop duplication");
                return Some(Box::new(d));
            }
            Err(e) => crate::plog!("[screen] DXGI duplication unavailable ({e}); falling back to xcap"),
        }
    }
    let monitor = xcap::Monitor::all().ok()?.into_iter().next()?;
    crate::plog!("[screen] capture: xcap");
    Some(Box::new(Xcap { monitor }))
}

struct Xcap {
    monitor: xcap::Monitor,
}

impl Capturer for Xcap {
    fn next(&mut self, _timeout: Duration) -> anyhow::Result<Option<Captured>> {
        let shot = self.monitor.capture_image()?;
        let (width, height) = (shot.width(), shot.height());
        Ok(Some(Captured { width, height, pixels: shot.into_raw(), bgra: false }))
    }

    fn origin(&self) -> (i32, i32) {
        (self.monitor.x(), self.monitor.y())
    }
}

#[cfg(windows)]
pub(crate) mod dxgi {
    use super::{Captured, Capturer};
    use anyhow::{anyhow, Result};
    use std::time::Duration;
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
        D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIFactory6, IDXGIOutput, IDXGIOutput1,
        IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
        DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_GPU_PREFERENCE_MINIMUM_POWER,
        DXGI_OUTDUPL_FRAME_INFO,
    };

    pub struct Dxgi {
        device: ID3D11Device,
        ctx: ID3D11DeviceContext,
        output: IDXGIOutput1,
        dup: Option<IDXGIOutputDuplication>,
        staging: Option<(ID3D11Texture2D, u32, u32)>,
        origin: (i32, i32),
        /// The next frame must be delivered even if duplication reports no
        /// new image (fresh duplication = a client that has nothing yet).
        need_full: bool,
    }

    impl Dxgi {
        /// Try each desktop output, on each adapter, until duplication works.
        /// Order matters on hybrid-GPU laptops: Windows may list the panel
        /// under the discrete GPU, but only the GPU actually scanning it out
        /// (usually the integrated one) can duplicate it — so lowest-power
        /// adapters first, and the primary output (0,0) before the others.
        pub fn new() -> Result<Self> {
            prefer_power_saving_gpu();
            let mut last = anyhow!("no desktop output");
            for (adapter, output) in unsafe { candidates()? } {
                let name = unsafe { adapter.GetDesc1() }
                    .map(|d| String::from_utf16_lossy(&d.Description).trim_end_matches(char::from(0)).to_string())
                    .unwrap_or_default();
                match unsafe { Self::on(&adapter, &output) } {
                    Ok(me) => {
                        crate::plog!("[screen] duplicating the desktop on {name}");
                        return Ok(me);
                    }
                    Err(e) => {
                        crate::plog!("[screen] duplication on {name} failed: {e}");
                        last = e;
                    }
                }
            }
            Err(last)
        }

        unsafe fn on(adapter: &IDXGIAdapter1, output: &IDXGIOutput) -> Result<Self> {
            let r = output.GetDesc()?.DesktopCoordinates;
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?;
            let mut me = Dxgi {
                device: device.ok_or_else(|| anyhow!("no d3d device"))?,
                ctx: ctx.ok_or_else(|| anyhow!("no d3d context"))?,
                output: output.cast()?,
                dup: None,
                staging: None,
                origin: (r.left, r.top),
                need_full: true,
            };
            me.duplicate()?;
            Ok(me)
        }

        fn duplicate(&mut self) -> Result<()> {
            self.dup = None;
            // A per-monitor-DPI-aware (v2) process gets DXGI_ERROR_UNSUPPORTED
            // from the old DuplicateOutput; DuplicateOutput1 (Win10 1703+) is
            // the one that works there.
            let dup = unsafe {
                match self.output.cast::<IDXGIOutput5>() {
                    Ok(o5) => o5.DuplicateOutput1(&self.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])?,
                    Err(_) => self.output.DuplicateOutput(&self.device)?,
                }
            };
            let fmt = unsafe { dup.GetDesc().ModeDesc.Format };
            if fmt != DXGI_FORMAT_B8G8R8A8_UNORM {
                // e.g. an HDR desktop in FP16 — not worth converting here
                return Err(anyhow!("desktop format {fmt:?} is not BGRA8"));
            }
            self.dup = Some(dup);
            self.need_full = true;
            Ok(())
        }

        fn staging(&mut self, w: u32, h: u32) -> Result<ID3D11Texture2D> {
            if let Some((t, sw, sh)) = &self.staging {
                if *sw == w && *sh == h {
                    return Ok(t.clone());
                }
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
            let tex = tex.ok_or_else(|| anyhow!("no staging texture"))?;
            self.staging = Some((tex.clone(), w, h));
            Ok(tex)
        }
    }

    impl Capturer for Dxgi {
        fn next(&mut self, timeout: Duration) -> Result<Option<Captured>> {
            if self.dup.is_none() {
                // lost earlier (mode change, UAC / lock screen): keep retrying
                if self.duplicate().is_err() {
                    std::thread::sleep(timeout);
                    return Ok(None);
                }
            }
            let dup = self.dup.clone().unwrap();
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            if let Err(e) = unsafe { dup.AcquireNextFrame(ms, &mut info, &mut res) } {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(None);
                }
                if e.code() == DXGI_ERROR_ACCESS_LOST {
                    self.dup = None;
                    return Ok(None);
                }
                return Err(e.into());
            }
            // From here the frame must be released whatever happens.
            let out = (|| -> Result<Option<Captured>> {
                if info.LastPresentTime == 0 && !self.need_full {
                    return Ok(None); // pointer-only update: the image didn't change
                }
                let tex: ID3D11Texture2D = res.as_ref().ok_or_else(|| anyhow!("no frame"))?.cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { tex.GetDesc(&mut desc) };
                let (w, h) = (desc.Width, desc.Height);
                let staging = self.staging(w, h)?;
                let mut pixels = vec![0u8; w as usize * h as usize * 4];
                unsafe {
                    self.ctx.CopyResource(&staging, &tex);
                    let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                    self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
                    let row = w as usize * 4;
                    let src = map.pData as *const u8;
                    for y in 0..h as usize {
                        std::ptr::copy_nonoverlapping(
                            src.add(y * map.RowPitch as usize),
                            pixels.as_mut_ptr().add(y * row),
                            row,
                        );
                    }
                    self.ctx.Unmap(&staging, 0);
                }
                self.need_full = false;
                Ok(Some(Captured { width: w, height: h, pixels, bgra: true }))
            })();
            unsafe {
                let _ = dup.ReleaseFrame();
            }
            out
        }

        fn origin(&self) -> (i32, i32) {
            self.origin
        }
    }

    /// On a hybrid-GPU laptop a process assigned the discrete GPU sees the
    /// panel as that GPU's output, and duplicating it there fails with
    /// DXGI_ERROR_UNSUPPORTED. Windows' per-app graphics preference
    /// ("Power saving", same as Settings > Display > Graphics) puts plaind on
    /// the integrated GPU that actually drives the panel. Set once, only if the
    /// user hasn't chosen a preference for this exe already.
    fn prefer_power_saving_gpu() {
        use windows::core::PCWSTR;
        use windows::Win32::System::Registry::{RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};
        let Ok(exe) = std::env::current_exe() else { return };
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let key = wide(r"Software\Microsoft\DirectX\UserGpuPreferences");
        let name = wide(&exe.to_string_lossy());
        let data = wide("GpuPreference=1;");
        unsafe {
            let set = RegGetValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, None, None);
            if set.is_ok() {
                return;
            }
            let r = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            );
            if r.is_ok() {
                crate::plog!("[screen] set this exe's GPU preference to power saving (for desktop duplication)");
            }
        }
    }

    /// Every desktop-attached (adapter, output) pair, lowest-power adapter
    /// first, the primary output (top-left at 0,0) first within that.
    unsafe fn candidates() -> Result<Vec<(IDXGIAdapter1, IDXGIOutput)>> {
        let mut adapters: Vec<IDXGIAdapter1> = Vec::new();
        if let Ok(f6) = CreateDXGIFactory1::<IDXGIFactory6>() {
            for a in 0.. {
                match f6.EnumAdapterByGpuPreference::<IDXGIAdapter1>(a, DXGI_GPU_PREFERENCE_MINIMUM_POWER) {
                    Ok(x) => adapters.push(x),
                    Err(_) => break,
                }
            }
        }
        if adapters.is_empty() {
            let f: IDXGIFactory1 = CreateDXGIFactory1()?;
            for a in 0.. {
                match f.EnumAdapters1(a) {
                    Ok(x) => adapters.push(x),
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let mut out = Vec::new();
        for adapter in adapters {
            for o in 0.. {
                let Ok(output) = adapter.EnumOutputs(o) else { break };
                let d = output.GetDesc()?;
                if d.AttachedToDesktop.as_bool() {
                    out.push((adapter.clone(), output, d.DesktopCoordinates));
                }
            }
        }
        // stable: keeps the power order among equally-primary outputs
        out.sort_by_key(|(_, _, r)| !(r.left == 0 && r.top == 0));
        Ok(out.into_iter().map(|(a, o, _)| (a, o)).collect())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::Capturer;

    /// Manual check on a desktop session: `cargo test --lib capture -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dxgi_captures_a_frame() {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let mut d = super::dxgi::Dxgi::new().expect("duplication");
        let t0 = std::time::Instant::now();
        let f = d.next(std::time::Duration::from_millis(500)).unwrap().expect("first frame");
        println!("{}x{} in {:?}", f.width, f.height, t0.elapsed());
        let t1 = std::time::Instant::now();
        let again = d.next(std::time::Duration::from_millis(50)).unwrap();
        println!("second: changed={} in {:?}", again.is_some(), t1.elapsed());
    }
}
