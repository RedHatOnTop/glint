//! Direct2D + DirectWrite + DirectComposition surface for one window
//! (SHELL_DESIGN §5 렌더링). The swapchain is a composition swapchain with
//! premultiplied alpha so the DWM acrylic backdrop shows through wherever we
//! draw translucent color.
//!
//! All windows share one GPU stack (D3D device, D2D device, DComp device,
//! DWrite/WIC factories) via a thread-local — with four windows alive the
//! per-window device cost was the biggest slice of the RAM budget overrun.

use std::cell::OnceCell;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Imaging::{CLSID_WICImagingFactory, IWICImagingFactory};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{Interface, Result, w};

#[derive(Clone)]
pub struct Gpu {
    pub d3d: ID3D11Device,
    pub d2d: ID2D1Device,
    pub dxgi: IDXGIFactory2,
    pub dcomp: IDCompositionDevice,
    pub dwrite: IDWriteFactory,
    pub wic: IWICImagingFactory,
}

thread_local! {
    static GPU: OnceCell<Gpu> = const { OnceCell::new() };
}

/// Shared GPU stack for this (UI) thread, created on first use.
pub fn gpu() -> Result<Gpu> {
    GPU.with(|cell| {
        if let Some(g) = cell.get() {
            return Ok(g.clone());
        }
        let g = Gpu::new()?;
        let _ = cell.set(g.clone());
        Ok(g)
    })
}

fn d3d_device(kind: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    unsafe {
        let mut d3d: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            kind,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut d3d),
            None,
            None,
        )?;
        Ok(d3d.unwrap())
    }
}

impl Gpu {
    fn new() -> Result<Self> {
        unsafe {
            // A shell does not get to pick its machine. A Hyper-V synthetic
            // adapter has no D3D11 hardware device at all, and a real box has
            // none for the seconds its GPU driver is being replaced — without a
            // fallback either one takes the whole desktop down at startup.
            // WARP is software but complete: D2D and DirectComposition both
            // run on it. Loud on the way down, because a silent WARP session
            // just looks like a machine that got slow.
            let d3d = match d3d_device(D3D_DRIVER_TYPE_HARDWARE) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("glide-shell: no D3D11 hardware device ({e}); falling back to WARP");
                    d3d_device(D3D_DRIVER_TYPE_WARP)?
                }
            };
            let dxgi_device: IDXGIDevice = d3d.cast()?;

            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d = d2d_factory.CreateDevice(&dxgi_device)?;

            let dxgi: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

            Ok(Gpu { d3d, d2d, dxgi, dcomp, dwrite, wic })
        }
    }
}

pub struct Renderer {
    _gpu: Gpu,
    pub dc: ID2D1DeviceContext,
    swapchain: IDXGISwapChain1,
    _dcomp_target: IDCompositionTarget,
    pub dwrite: IDWriteFactory,
    pub fmt_title: IDWriteTextFormat,
    pub fmt_clock: IDWriteTextFormat,
    pub fmt_date: IDWriteTextFormat,
    pub fmt_glyph: IDWriteTextFormat,
    pub fmt_status: IDWriteTextFormat,
    pub dpi: f32,
}

impl Renderer {
    pub fn new(hwnd: HWND, width: u32, height: u32, dpi: f32) -> Result<Self> {
        unsafe {
            let gpu = gpu()?;
            let dc = gpu.d2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swapchain = gpu.dxgi.CreateSwapChainForComposition(&gpu.d3d, &desc, None)?;

            let dcomp_target = gpu.dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = gpu.dcomp.CreateVisual()?;
            visual.SetContent(&swapchain)?;
            dcomp_target.SetRoot(&visual)?;
            gpu.dcomp.Commit()?;

            let dwrite = gpu.dwrite.clone();
            let mk = |family: windows::core::PCWSTR, size: f32, weight: DWRITE_FONT_WEIGHT| {
                dwrite.CreateTextFormat(
                    family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("ko-kr"),
                )
            };
            // DWrite falls back to system fonts for Hangul automatically, so a
            // single Segoe UI Variable family is enough (unlike GDI).
            let family = w!("Segoe UI Variable");
            let fmt_title = mk(family, 12.5, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe UI"), 12.5, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_title.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_title.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            // Window titles are arbitrarily long; the task button is not.
            ellipsize(&dwrite, &fmt_title);
            let fmt_clock = mk(family, 13.5, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                .or_else(|_| mk(w!("Segoe UI"), 13.5, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
            fmt_clock.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            let fmt_date = mk(family, 10.5, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe UI"), 10.5, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_date.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            // Status cluster: Fluent glyphs (MDL2 codepoints) + 한/A letter.
            let fmt_glyph = mk(w!("Segoe Fluent Icons"), 13.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe MDL2 Assets"), 13.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_glyph.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_glyph.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_status = mk(family, 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)
                .or_else(|_| mk(w!("Segoe UI"), 12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD))?;
            fmt_status.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_status.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            let mut r = Renderer {
                _gpu: gpu,
                dc,
                swapchain,
                _dcomp_target: dcomp_target,
                dwrite,
                fmt_title,
                fmt_clock,
                fmt_date,
                fmt_glyph,
                fmt_status,
                dpi,
            };
            r.bind_backbuffer()?;
            Ok(r)
        }
    }

    unsafe fn bind_backbuffer(&mut self) -> Result<()> {
        unsafe {
            let surface: IDXGISurface = self.swapchain.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: self.dpi,
                dpiY: self.dpi,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                ..Default::default()
            };
            let bitmap = self.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            self.dc.SetTarget(&bitmap);
            self.dc.SetDpi(self.dpi, self.dpi);
            Ok(())
        }
    }

    pub fn resize(&mut self, width: u32, height: u32, dpi: f32) -> Result<()> {
        unsafe {
            self.dpi = dpi;
            self.dc.SetTarget(None::<&ID2D1Image>);
            self.swapchain.ResizeBuffers(
                2,
                width.max(1),
                height.max(1),
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
            self.bind_backbuffer()
        }
    }

    pub fn brush(&self, color: D2D1_COLOR_F) -> Result<ID2D1SolidColorBrush> {
        unsafe { self.dc.CreateSolidColorBrush(&color, None) }
    }

    /// Natural pixel width of `text` in the given format (for tight buttons).
    pub fn text_width(&self, text: &[u16], fmt: &IDWriteTextFormat, max_w: f32) -> f32 {
        unsafe {
            let layout = match self.dwrite.CreateTextLayout(text, fmt, max_w, 100.0) {
                Ok(l) => l,
                Err(_) => return max_w,
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_ok() { m.widthIncludingTrailingWhitespace } else { max_w }
        }
    }

    pub fn present(&self) -> Result<()> {
        unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)).ok() }
    }
}

/// End overflowing text with an ellipsis instead of a hard clip.
///
/// Everything in the shell draws into a fixed cell — a task button, an app row,
/// a notification body, a device name. Without a trimming sign D2D just clips at
/// the rect edge, which cuts a Hangul syllable apart into a stray jamo and gives
/// no sign that anything is missing. Character granularity, not word: Korean
/// rarely offers a word break near the edge.
pub fn ellipsize(dwrite: &IDWriteFactory, fmt: &IDWriteTextFormat) {
    unsafe {
        let Ok(sign) = dwrite.CreateEllipsisTrimmingSign(fmt) else { return };
        let _ = fmt.SetTrimming(
            &DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0,
                delimiterCount: 0,
            },
            &sign,
        );
    }
}

/// A D2D rect from four edges. Every painter in the shell wants this, and each
/// one used to carry its own copy.
pub fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F { left, top, right, bottom }
}

/// Fill a rounded rectangle. Radius 0 takes the plain-rectangle path, which is
/// what the hairlines and separators pass.
pub fn fill_round(r: &Renderer, rc: D2D_RECT_F, radius: f32, color: D2D1_COLOR_F) {
    unsafe {
        if let Ok(b) = r.brush(color) {
            if radius > 0.0 {
                r.dc.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: rc, radiusX: radius, radiusY: radius },
                    &b,
                );
            } else {
                r.dc.FillRectangle(&rc, &b);
            }
        }
    }
}
