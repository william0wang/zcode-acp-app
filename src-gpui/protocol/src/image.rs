//! Client-side image-attachment preparation — the Rust port of the TS app's
//! `src/lib/image.ts` (ADR 0007 downsampling). Semantic parity is kept: the
//! same limits (MAX_IMAGES, MAX_EDGE, quality, passthrough/re-encode
//! thresholds), the same animated/vector passthrough rule, and the same wire
//! shape (base64 without the data-url prefix — exactly the ACP image block
//! payload the server's `extractAttachments` consumes).
//!
//! Two deliberate divergences, both documented here:
//! - the browser canvas re-encodes alpha images as LOSSY WebP; the pure-Rust
//!   `image` stack only offers lossless VP8L — same `image/webp` mime, alpha
//!   preserved, size still bounded by the resample (server cap is 10 MiB per
//!   inline image, `zcode-acp-server` `handlers/prompt-content.ts`).
//! - the TS alpha scan samples ~16k pixels and can miss transparency; this
//!   port scans every pixel, so an alpha image never flattens to JPEG black.
//!
//! Wire formats are restricted to the server's accepted inline set
//! (`imageTypes` in prompt-content.ts): png / jpeg / webp / gif / bmp /
//! svg+xml. Decodable-but-unaccepted inputs (tiff, ico, …) always re-encode
//! instead of passing through — the TS app never sees those mimes from a
//! browser file picker, a native drop does.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use image::imageops::FilterType;
use image::{GenericImageView, ImageEncoder as _};

use crate::chat::AttachmentDraft;

/// Max images per prompt (TS `MAX_IMAGES`); enforced by the composer's
/// staging, surfaced as a notice when hit.
pub const MAX_IMAGES: usize = 4;
/// Longest edge after resampling — the common multimodal-API safe value.
const MAX_EDGE: u32 = 1568;
const JPEG_QUALITY: u8 = 85;
// Animated/vector formats must not pass through a rasterizer (frame/vector
// loss); small ones ride as-is, big ones are rejected.
const PASSTHROUGH_MAX_BYTES: usize = 5 * 1024 * 1024;
// Re-encoding only pays off past this size; smaller originals stay untouched.
const REENCODE_MIN_BYTES: usize = 300 * 1024;
/// MIME types the server accepts inline (`imageTypes`, prompt-content.ts).
const INLINE_MIMES: [&str; 6] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/bmp",
    "image/svg+xml",
];

/// Failure reasons, the TS `PrepareResult` `{ok:false}` spellings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareError {
    TooLarge,
    Unsupported,
}

/// The TS `prepareImage`: downsample + re-encode one picked/pasted/dropped
/// image into an ACP-ready attachment. `mime` may be empty or carry
/// parameters; the bytes are sniffed regardless, like `createImageBitmap`.
pub fn prepare_image(bytes: &[u8], mime: &str) -> Result<AttachmentDraft, PrepareError> {
    let mime = normalize_mime(mime);
    let is = |want: &str| mime.eq_ignore_ascii_case(want);
    let canonical = || canonical_inline(mime).unwrap_or(mime);
    if is("image/gif") || is("image/svg+xml") {
        if bytes.len() > PASSTHROUGH_MAX_BYTES {
            return Err(PrepareError::TooLarge);
        }
        return Ok(AttachmentDraft {
            data: BASE64.encode(bytes),
            mime_type: canonical().to_string(),
        });
    }

    let decoded = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| PrepareError::Unsupported)?
        .decode()
        .map_err(|_| PrepareError::Unsupported)?;

    let (w, h) = decoded.dimensions();
    let scale = MAX_EDGE as f64 / w.max(h) as f64;
    // Case-insensitive like the server's own validator (prompt-content.ts
    // `mime()` lowercases before the imageTypes check).
    let inline_legal = canonical_inline(mime).is_some();
    if scale >= 1.0 && bytes.len() <= REENCODE_MIN_BYTES && inline_legal {
        // Already small enough and wire-legal: ride as-is (the TS early-out).
        return Ok(AttachmentDraft {
            data: BASE64.encode(bytes),
            mime_type: canonical().to_string(),
        });
    }

    let resized = if scale < 1.0 {
        let tw = ((w as f64 * scale).round() as u32).max(1);
        let th = ((h as f64 * scale).round() as u32).max(1);
        // Canvas `drawImage` resampling ≈ a cubic filter; CatmullRom matches
        // its sharpness without Lanczos3's cost.
        decoded.resize_exact(tw, th, FilterType::CatmullRom)
    } else {
        decoded
    };
    let (tw, th) = resized.dimensions();

    let rgba = resized.to_rgba8();
    let alpha = rgba.pixels().any(|p| p.0[3] < 255);
    let (data, mime_type) = if alpha {
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .write_image(
                rgba.as_raw(),
                tw,
                th,
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|_| PrepareError::Unsupported)?;
        (out, "image/webp")
    } else {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
            .write_image(
                resized.to_rgb8().as_raw(),
                tw,
                th,
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|_| PrepareError::Unsupported)?;
        (out, "image/jpeg")
    };
    Ok(AttachmentDraft {
        data: BASE64.encode(data),
        mime_type: mime_type.to_string(),
    })
}

/// `data:{mime};base64,{payload}` — the display form the chat model keeps
/// (TS `attachmentDataUrl`); `SessionCore::run_prompt` uses the same spelling.
pub fn attachment_data_url(img: &AttachmentDraft) -> String {
    format!("data:{};base64,{}", img.mime_type, img.data)
}

/// Inverse of [`attachment_data_url`] — parses a stored echo back into a
/// draft (pure string shape check; no decode).
pub fn parse_data_url(url: &str) -> Option<AttachmentDraft> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(",")?;
    let mime = meta.strip_suffix(";base64")?;
    if data.is_empty() {
        return None;
    }
    Some(AttachmentDraft {
        data: data.to_string(),
        mime_type: mime.to_string(),
    })
}

/// Decodes a draft's base64 payload back to bytes (UI thumbnails render the
/// raster directly; the wire never sees this direction).
pub fn decode_attachment_bytes(img: &AttachmentDraft) -> Option<Vec<u8>> {
    BASE64.decode(img.data.as_bytes()).ok()
}

/// Sniffs a mime type from bytes when the source has none (a native drop or
/// an extension-less file). SVG is XML text the `image` sniffer cannot see,
/// so it gets a small header scan of its own.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if let Ok(format) = image::guess_format(bytes) {
        return Some(mime_of(format));
    }
    let head: &[u8] = &bytes[..bytes.len().min(512)];
    if head.starts_with(b"<") || head.starts_with(b"\xEF\xBB\xBF<") {
        let text = String::from_utf8_lossy(head).to_lowercase();
        if text.contains("<svg") {
            return Some("image/svg+xml");
        }
    }
    None
}

/// image::ImageFormat → the canonical inline mime spelling.
pub fn mime_of(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::Bmp => "image/bmp",
        image::ImageFormat::Tiff => "image/tiff",
        image::ImageFormat::Ico => "image/x-icon",
        other => {
            // Fall back to the image crate's own mapping; its spellings for
            // the exotic formats are informational only (they re-encode).
            let _ = other;
            "application/octet-stream"
        }
    }
}

/// Canonical spelling for a server-inline mime (case-insensitive match),
/// mirroring the server validator's lowercasing (`mime()` in
/// prompt-content.ts).
fn canonical_inline(mime: &str) -> Option<&'static str> {
    INLINE_MIMES.iter().copied().find(|m| mime.eq_ignore_ascii_case(m))
}

/// `file.type || "image/png"` semantics: strip parameters and fold the common
/// `image/jpg` alias. Case is left as-is — every internal caller passes a
/// canonical lowercase mime (sniffed, extension-mapped, or from a browser
/// clipboard), and a non-canonical exotic input merely re-encodes.
fn normalize_mime(mime: &str) -> &str {
    let bare = mime.split(';').next().unwrap_or("").trim();
    match bare {
        "" => "image/png",
        _ if bare.eq_ignore_ascii_case("image/jpg") => "image/jpeg",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    /// Noisy RGB buffer compresses badly on purpose so the PNG crosses the
    /// re-encode threshold.
    fn noisy_png(w: u32, h: u32) -> Vec<u8> {
        let img = ImageBuffer::from_fn(w, h, |x, y| {
            let v = (x * 31 + y * 17) as u8 ^ (x * y) as u8;
            image::Rgb([v, v.wrapping_mul(3), 255 - v])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn rgba_png(w: u32, h: u32, alpha: u8) -> Vec<u8> {
        let img = ImageBuffer::from_fn(w, h, |x, y| {
            Rgba([x as u8, y as u8, 128, alpha])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn decode_dims(draft: &AttachmentDraft) -> (u32, u32, image::ImageFormat) {
        let bytes = decode_attachment_bytes(draft).unwrap();
        let format = image::guess_format(&bytes).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        (img.width(), img.height(), format)
    }

    #[test]
    fn max_images_matches_ts() {
        assert_eq!(MAX_IMAGES, 4);
    }

    #[test]
    fn big_opaque_png_becomes_bounded_jpeg() {
        let png = noisy_png(2000, 1500);
        assert!(png.len() > REENCODE_MIN_BYTES);
        let draft = prepare_image(&png, "image/png").unwrap();
        assert_eq!(draft.mime_type, "image/jpeg");
        let (w, h, format) = decode_dims(&draft);
        assert_eq!(format, image::ImageFormat::Jpeg);
        assert_eq!(w.max(h), MAX_EDGE);
        // Downsampled JPEG must be materially smaller than a raw 2000x1500.
        assert!(decode_attachment_bytes(&draft).unwrap().len() < png.len());
    }

    #[test]
    fn small_png_rides_as_is() {
        let png = rgba_png(64, 48, 255);
        let draft = prepare_image(&png, "image/png").unwrap();
        assert_eq!(draft.mime_type, "image/png");
        assert_eq!(decode_attachment_bytes(&draft).unwrap(), png);
    }

    #[test]
    fn big_alpha_png_becomes_webp() {
        let png = rgba_png(1800, 1200, 128);
        let draft = prepare_image(&png, "image/png").unwrap();
        assert_eq!(draft.mime_type, "image/webp");
        let (w, h, format) = decode_dims(&draft);
        assert_eq!(format, image::ImageFormat::WebP);
        assert!(w.max(h) <= MAX_EDGE);
        // Transparency survives the round trip.
        let decoded = image::load_from_memory(&decode_attachment_bytes(&draft).unwrap()).unwrap();
        assert!(decoded.to_rgba8().pixels().any(|p| p.0[3] < 255));
    }

    #[test]
    fn gif_passes_through_when_small_and_rejects_when_huge() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend(std::iter::repeat_n(0u8, 1024));
        let draft = prepare_image(&gif, "image/gif").unwrap();
        assert_eq!(draft.mime_type, "image/gif");

        let mut huge = b"GIF89a".to_vec();
        huge.resize(PASSTHROUGH_MAX_BYTES + 1, 0);
        assert_eq!(prepare_image(&huge, "image/gif"), Err(PrepareError::TooLarge));
    }

    #[test]
    fn undecodable_bytes_are_unsupported() {
        assert_eq!(
            prepare_image(b"not an image at all", "image/png"),
            Err(PrepareError::Unsupported)
        );
    }

    #[test]
    fn tiff_sized_small_still_reencodes_to_inline_mime() {
        // TIFF is decodable but not inline-accepted, so it must re-encode
        // even when small (the passthrough whitelist holds the line).
        let img = ImageBuffer::from_fn(64, 64, |x, y| image::Rgb([x as u8, y as u8, 7]));
        let mut tiff = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut tiff), image::ImageFormat::Tiff)
            .unwrap();
        assert!(tiff.len() <= REENCODE_MIN_BYTES);
        let draft = prepare_image(&tiff, "image/tiff").unwrap();
        assert_eq!(draft.mime_type, "image/jpeg");
    }

    #[test]
    fn jpg_alias_and_default_mime_normalize() {
        let png = rgba_png(32, 32, 255);
        // Passthrough keeps the DECLARED (normalized) mime, like the TS
        // `file.type || "image/png"` — sniffing only steers re-encoding.
        let draft = prepare_image(&png, "image/jpg").unwrap();
        assert_eq!(draft.mime_type, "image/jpeg");
        let draft = prepare_image(&png, "").unwrap();
        assert_eq!(draft.mime_type, "image/png");
        let draft = prepare_image(&png, "image/PNG;charset=utf-8").unwrap();
        assert_eq!(draft.mime_type, "image/png");
    }

    #[test]
    fn data_url_round_trips() {
        let draft = AttachmentDraft {
            data: BASE64.encode(b"payload"),
            mime_type: "image/png".to_string(),
        };
        let url = attachment_data_url(&draft);
        assert_eq!(url, "data:image/png;base64,cGF5bG9hZA==");
        assert_eq!(parse_data_url(&url).unwrap(), draft);
        assert!(parse_data_url("https://example.com/x.png").is_none());
    }

    #[test]
    fn sniff_recognizes_png_and_svg() {
        assert_eq!(sniff_mime(&noisy_png(8, 8)), Some("image/png"));
        assert_eq!(
            sniff_mime(b"<?xml version=\"1.0\"?>\n<svg xmlns=\"a\"></svg>"),
            Some("image/svg+xml")
        );
        assert_eq!(sniff_mime(b"garbage"), None);
    }
}
