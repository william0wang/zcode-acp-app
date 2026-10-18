//! Self-capture for the golden pixel stage (the acceptance script's stage
//! 6): the process screenshots its OWN window through CoreGraphics.
//! Capturing your own window is TCC-exempt on macOS — `screencapture` from
//! a terminal would need Screen Recording permission, this path needs
//! nothing, is immune to occlusion, and needs no region math.
//!
//! CAVEAT (found in practice): gpui renders into a Metal surface the
//! window server cannot always read back, in which case the capture comes
//! out fully transparent. The capture therefore reports an EMPTY picture
//! (all alpha 0) as `Err(kind == Empty)` — the script falls back to
//! `screencapture -R` over the pinned window frame (that route needs the
//! terminal to hold Screen Recording permission).
//!
//! Flow: walk the window list for a window owned by THIS pid,
//! `CGWindowListCreateImage` it at the window frame only
//! (BoundsIgnoreFraming — no shadow margins, matching the fallback's
//! region), convert the premultiplied-BGRA bitmap to RGBA, encode PNG via
//! the `image` pin.

// CGWindowListCreateImage is deprecated in favor of ScreenCaptureKit, but
// SCK is an async session framework — massive overkill for one self-shot;
// the CG call remains fully functional for own-window capture.
#![allow(deprecated)]

use std::path::Path;

use objc2_core_foundation::{CFDictionary, CFNumber, CFNumberType, CFString};
use objc2_core_graphics::{
    kCGWindowNumber, kCGWindowOwnerPID, CGDataProviderCopyData, CGImage, CGRectNull,
    CGWindowImageOption, CGWindowListCopyWindowInfo, CGWindowListCreateImage,
    CGWindowListOption,
};

/// Why a capture failed — `Empty` is the Metal-readback case the script
/// knows how to retry with `screencapture`.
#[derive(Debug)]
pub enum CaptureError {
    Empty,
    Other(String),
}

/// Find our own window id (owner pid match, positive number — 0 is the
/// desktop's placeholder).
fn own_window_id() -> Option<u32> {
    let pid = i64::from(std::process::id());
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0)?;
    let key = |k: &'static CFString| (k as *const CFString).cast::<std::ffi::c_void>();
    for i in 0..list.count() {
        // The generated array is untyped at the seams — each entry is an
        // opaque CFDictionary (CFString keys → CFType values); the raw
        // `value` accessor lives on the default instantiation.
        let dict = unsafe { list.value_at_index(i) as *const CFDictionary };
        let dict = unsafe { &*dict };
        let read_i64 = |k: &'static CFString| -> Option<i64> {
            let value = unsafe { dict.value(key(k)) };
            if value.is_null() {
                return None;
            }
            let number = unsafe { &*(value as *const CFNumber) };
            let mut out: i64 = 0;
            let ok = unsafe {
                number.value(CFNumberType::SInt64Type, &mut out as *mut i64 as *mut _)
            };
            ok.then_some(out)
        };
        if read_i64(unsafe { kCGWindowOwnerPID }) != Some(pid) {
            continue;
        }
        if let Some(number) = read_i64(unsafe { kCGWindowNumber }) {
            if number > 0 {
                return Some(number as u32);
            }
        }
    }
    None
}

/// Capture our own window and write it as PNG (the window frame only —
/// shadow margins excluded, so the geometry matches the script's
/// `screencapture -R120,120,1200,828` fallback).
pub fn capture_own_window_png(out: &Path) -> Result<(), CaptureError> {
    let other = |e: String| Err(CaptureError::Other(e));
    let window_id = own_window_id()
        .ok_or_else(|| CaptureError::Other("no on-screen window owned by this process".into()))?;
    let image = CGWindowListCreateImage(
        unsafe { CGRectNull },
        CGWindowListOption::OptionIncludingWindow,
        window_id,
        // The window's own frame, no shadow margin — same rectangle the
        // fallback captures.
        CGWindowImageOption::BoundsIgnoreFraming,
    )
    .ok_or_else(|| CaptureError::Other("CGWindowListCreateImage returned nothing".into()))?;

    let (width, height) = (CGImage::width(Some(&image)), CGImage::height(Some(&image)));
    if width == 0 || height == 0 {
        return other("empty window capture".into());
    }
    let bits_per_pixel = CGImage::bits_per_pixel(Some(&image));
    if bits_per_pixel != 32 {
        return Err(CaptureError::Other(format!(
            "unexpected {bits_per_pixel} bpp capture"
        )));
    }
    let bytes_per_row = CGImage::bytes_per_row(Some(&image));
    let provider = CGImage::data_provider(Some(&image))
        .ok_or_else(|| CaptureError::Other("no data provider".into()))?;
    let data = CGDataProviderCopyData(Some(&provider))
        .ok_or_else(|| CaptureError::Other("no bitmap bytes".into()))?;
    let (len, ptr) = (data.length() as usize, data.byte_ptr());
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };

    // The capture is premultiplied BGRA (little-endian ARGB); an opaque
    // canvas makes premultiplication the identity — swap B and R into the
    // RGBA order the PNG encoder expects, honoring the row stride.
    let mut rgba = Vec::with_capacity(width * height * 4);
    let mut opaque = 0usize;
    for y in 0..height {
        let row = &bytes[y * bytes_per_row..y * bytes_per_row + width * 4];
        for px in row.chunks_exact(4) {
            if px[3] != 0 {
                opaque += 1;
            }
            rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    // The Metal-readback case: the window reads out fully transparent.
    if opaque * 100 < width * height {
        return Err(CaptureError::Empty);
    }
    let buffer = image::RgbaImage::from_raw(width as u32, height as u32, rgba)
        .ok_or_else(|| CaptureError::Other("bitmap size mismatch".into()))?;
    buffer
        .save(out)
        .map_err(|e| CaptureError::Other(format!("png encode: {e}")))
}
