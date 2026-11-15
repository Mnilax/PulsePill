//! Thin immediate-mode wrapper over the GDI+ flat API (gdiplus.dll ships with Windows).
//! Draws antialiased shapes and text straight into the premultiplied BGRA buffer that
//! `UpdateLayeredWindow` composites, so per-pixel transparency comes for free.

use std::cell::RefCell;
use windows_sys::Win32::Graphics::GdiPlus::*;

pub type Argb = u32;

pub fn with_alpha(c: Argb, a: f32) -> Argb {
    let base = (c >> 24) as f32;
    (((base * a.clamp(0.0, 1.0)).round() as u32) << 24) | (c & 0x00FF_FFFF)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w - 2.0 * dx, self.h - 2.0 * dy)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Weight {
    Regular,
    Semibold,
    Bold,
}

struct FontCache {
    families: [*mut GpFontFamily; 2], // regular, semibold
    fonts: Vec<(u32, Weight, *mut GpFont)>,
}

thread_local! {
    static FONTS: RefCell<Option<FontCache>> = const { RefCell::new(None) };
}

fn family(names: &[&str]) -> *mut GpFontFamily {
    for n in names {
        let w: Vec<u16> = n.encode_utf16().chain(Some(0)).collect();
        let mut f = std::ptr::null_mut();
        if unsafe { GdipCreateFontFamilyFromName(w.as_ptr(), std::ptr::null_mut(), &mut f) } == 0 {
            return f;
        }
    }
    std::ptr::null_mut()
}

fn font(size: f32, weight: Weight) -> *mut GpFont {
    FONTS.with(|cell| {
        let mut cache = cell.borrow_mut();
        let cache = cache.get_or_insert_with(|| FontCache {
            families: [family(&["Segoe UI Variable Text", "Segoe UI"]), family(&["Segoe UI Variable Text Semibold", "Segoe UI Semibold", "Segoe UI"])],
            fonts: Vec::new(),
        });
        let key = (size * 4.0).round() as u32;
        if let Some(&(_, _, f)) = cache.fonts.iter().find(|(k, w, _)| *k == key && *w == weight) {
            return f;
        }
        let (fam, style) = match weight {
            Weight::Regular => (cache.families[0], 0),
            Weight::Semibold => (cache.families[1], 0),
            Weight::Bold => (cache.families[0], 1),
        };
        let mut f = std::ptr::null_mut();
        unsafe { GdipCreateFont(fam, size, style, UnitPixel, &mut f) };
        cache.fonts.push((key, weight, f));
        f
    })
}

pub struct Gfx {
    g: *mut GpGraphics,
    bitmap: *mut GpBitmap,
    formats: [*mut GpStringFormat; 3],
    /// Multiplies every color's alpha (used to fade content in and out).
    pub alpha: f32,
}

impl Gfx {
    /// Wraps a top-down premultiplied BGRA buffer. `scale` maps DIPs to pixels.
    pub unsafe fn new(bits: *mut u8, w: i32, h: i32, scale: f32) -> Gfx {
        let mut bitmap = std::ptr::null_mut();
        GdipCreateBitmapFromScan0(w, h, w * 4, 0x000E_200B /* PixelFormat32bppPARGB */, bits, &mut bitmap);
        let mut g = std::ptr::null_mut();
        GdipGetImageGraphicsContext(bitmap as *mut GpImage, &mut g);
        GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        GdipSetPixelOffsetMode(g, PixelOffsetModeHalf);
        GdipSetTextRenderingHint(g, TextRenderingHintAntiAliasGridFit);
        GdipSetCompositingQuality(g, CompositingQualityHighSpeed);
        GdipScaleWorldTransform(g, scale, scale, MatrixOrderPrepend);
        let mut typographic = std::ptr::null_mut();
        GdipStringFormatGetGenericTypographic(&mut typographic);
        let mut formats = [std::ptr::null_mut(); 3];
        for (i, align) in [StringAlignmentNear, StringAlignmentCenter, StringAlignmentFar].into_iter().enumerate() {
            GdipCloneStringFormat(typographic, &mut formats[i]);
            GdipSetStringFormatAlign(formats[i], align);
            GdipSetStringFormatLineAlign(formats[i], StringAlignmentCenter);
            GdipSetStringFormatTrimming(formats[i], StringTrimmingEllipsisCharacter);
            GdipSetStringFormatFlags(formats[i], (StringFormatFlagsNoWrap | StringFormatFlagsMeasureTrailingSpaces) as i32);
        }
        Gfx { g, bitmap, formats, alpha: 1.0 }
    }

    pub fn clear(&self) {
        unsafe { GdipGraphicsClear(self.g, 0) };
    }

    fn c(&self, c: Argb) -> Argb {
        if self.alpha >= 1.0 { c } else { with_alpha(c, self.alpha) }
    }

    fn with_brush(&self, color: Argb, f: impl FnOnce(*mut GpBrush)) {
        let mut b = std::ptr::null_mut();
        unsafe {
            GdipCreateSolidFill(self.c(color), &mut b);
            f(b as *mut GpBrush);
            GdipDeleteBrush(b as *mut GpBrush);
        }
    }

    fn with_pen(&self, color: Argb, width: f32, f: impl FnOnce(*mut GpPen)) {
        let mut p = std::ptr::null_mut();
        unsafe {
            GdipCreatePen1(self.c(color), width, UnitWorld, &mut p);
            GdipSetPenStartCap(p, LineCapRound);
            GdipSetPenEndCap(p, LineCapRound);
            GdipSetPenLineJoin(p, LineJoinRound);
            f(p);
            GdipDeletePen(p);
        }
    }

    fn round_path(r: Rect, radius: f32) -> *mut GpPath {
        let mut path = std::ptr::null_mut();
        let rad = radius.min(r.w / 2.0).min(r.h / 2.0).max(0.01);
        let d = rad * 2.0;
        unsafe {
            GdipCreatePath(FillModeAlternate, &mut path);
            GdipAddPathArc(path, r.x, r.y, d, d, 180.0, 90.0);
            GdipAddPathArc(path, r.right() - d, r.y, d, d, 270.0, 90.0);
            GdipAddPathArc(path, r.right() - d, r.bottom() - d, d, d, 0.0, 90.0);
            GdipAddPathArc(path, r.x, r.bottom() - d, d, d, 90.0, 90.0);
            GdipClosePathFigure(path);
        }
        path
    }

    pub fn fill_round(&self, r: Rect, radius: f32, color: Argb) {
        let path = Self::round_path(r, radius);
        self.with_brush(color, |b| unsafe {
            GdipFillPath(self.g, b, path);
        });
        unsafe { GdipDeletePath(path) };
    }

    pub fn stroke_round(&self, r: Rect, radius: f32, color: Argb, width: f32) {
        let path = Self::round_path(r, radius);
        self.with_pen(color, width, |p| unsafe {
            GdipDrawPath(self.g, p, path);
        });
        unsafe { GdipDeletePath(path) };
    }

    pub fn fill_circle(&self, cx: f32, cy: f32, d: f32, color: Argb) {
        self.with_brush(color, |b| unsafe {
            GdipFillEllipse(self.g, b, cx - d / 2.0, cy - d / 2.0, d, d);
        });
    }

    pub fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Argb) {
        self.with_pen(color, width, |p| unsafe {
            GdipDrawLine(self.g, p, x1, y1, x2, y2);
        });
    }

    pub fn polyline(&self, pts: &[PointF], width: f32, color: Argb) {
        if pts.len() < 2 {
            return;
        }
        self.with_pen(color, width, |p| unsafe {
            GdipDrawLines(self.g, p, pts.as_ptr(), pts.len() as i32);
        });
    }

    /// Fills the polygon with a vertical gradient from `top` to transparent at `bottom_y`.
    pub fn fill_area(&self, pts: &[PointF], top_y: f32, bottom_y: f32, color: Argb) {
        if pts.len() < 3 {
            return;
        }
        unsafe {
            let mut path = std::ptr::null_mut();
            GdipCreatePath(FillModeAlternate, &mut path);
            GdipAddPathLine2(path, pts.as_ptr(), pts.len() as i32);
            GdipClosePathFigure(path);
            let (p1, p2) = (PointF { X: 0.0, Y: top_y - 0.5 }, PointF { X: 0.0, Y: bottom_y + 0.5 });
            let mut brush = std::ptr::null_mut();
            GdipCreateLineBrush(&p1, &p2, self.c(color), self.c(color) & 0x00FF_FFFF, WrapModeTileFlipXY, &mut brush);
            GdipFillPath(self.g, brush as *mut GpBrush, path);
            GdipDeleteBrush(brush as *mut GpBrush);
            GdipDeletePath(path);
        }
    }

    pub fn text(&self, s: &str, r: Rect, size: f32, weight: Weight, color: Argb, align: Align) {
        if s.is_empty() {
            return;
        }
        let w: Vec<u16> = s.encode_utf16().collect();
        let fmt = self.formats[align as usize];
        let rf = RectF { X: r.x, Y: r.y, Width: r.w.max(1.0), Height: r.h };
        self.with_brush(color, |b| unsafe {
            GdipDrawString(self.g, w.as_ptr(), w.len() as i32, font(size, weight), &rf, fmt, b);
        });
    }

    pub fn measure(&self, s: &str, size: f32, weight: Weight) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        let w: Vec<u16> = s.encode_utf16().collect();
        let layout = RectF { X: 0.0, Y: 0.0, Width: 10_000.0, Height: 100.0 };
        let mut out = RectF { X: 0.0, Y: 0.0, Width: 0.0, Height: 0.0 };
        unsafe {
            GdipMeasureString(self.g, w.as_ptr(), w.len() as i32, font(size, weight), &layout, self.formats[0], &mut out, std::ptr::null_mut(), std::ptr::null_mut());
        }
        out.Width.ceil()
    }

    pub fn clip(&self, r: Rect) {
        unsafe { GdipSetClipRect(self.g, r.x, r.y, r.w, r.h, CombineModeReplace) };
    }

    pub fn reset_clip(&self) {
        unsafe { GdipResetClip(self.g) };
    }

    /// Writes the buffer as a PNG with alpha (used for README screenshots).
    pub fn save_png(&self, path: &str) -> bool {
        let png = windows_sys::core::GUID::from_u128(0x557cf406_1a04_11d3_9a73_0000f81ef32e);
        let p: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        unsafe {
            GdipFlush(self.g, FlushIntentionSync);
            GdipSaveImageToFile(self.bitmap as *mut GpImage, p.as_ptr(), &png, std::ptr::null()) == 0
        }
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe {
            for f in self.formats {
                GdipDeleteStringFormat(f);
            }
            GdipDeleteGraphics(self.g);
            GdipDisposeImage(self.bitmap as *mut GpImage);
        }
    }
}

pub fn startup() {
    let input = GdiplusStartupInput { GdiplusVersion: 1, DebugEventCallback: 0, SuppressBackgroundThread: 0, SuppressExternalCodecs: 1 };
    let mut token = 0usize;
    unsafe { GdiplusStartup(&mut token, &input, std::ptr::null_mut()) };
}
