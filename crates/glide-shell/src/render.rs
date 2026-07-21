//! Direct2D + DirectWrite + DirectComposition surface for one window
//! (SHELL_DESIGN §5 렌더링). The swapchain is a composition swapchain with
//! premultiplied alpha so the DWM acrylic backdrop shows through wherever we
//! draw translucent color.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, Result, w};

pub struct Renderer {
    _d3d: ID3D11Device,
    pub dc: ID2D1DeviceContext,
    swapchain: IDXGISwapChain1,
    _dcomp: IDCompositionDevice,
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
            let mut d3d: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                None,
            )?;
            let d3d = d3d.unwrap();
            let dxgi_device: IDXGIDevice = d3d.cast()?;

            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d_device = d2d_factory.CreateDevice(&dxgi_device)?;
            let dc = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;

            let dxgi_factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
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
            let swapchain = dxgi_factory.CreateSwapChainForComposition(&d3d, &desc, None)?;

            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
            let dcomp_target = dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swapchain)?;
            dcomp_target.SetRoot(&visual)?;
            dcomp.Commit()?;

            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
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
                _d3d: d3d,
                dc,
                swapchain,
                _dcomp: dcomp,
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
