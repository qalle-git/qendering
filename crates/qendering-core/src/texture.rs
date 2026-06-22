//! Decode a parsed texture to pixels and encode a square preview image.
//!
//! Pipeline: [`TextureInfo`] -> DDS bytes ([`crate::dds`]) -> RGBA (via
//! `image_dds`, which decodes BC1/2/3/4/5/7) -> resized square canvas ->
//! WebP / PNG / JPEG bytes.

use std::io::Cursor;
use std::path::Path;

use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

use crate::dds::build_dds;
use crate::error::{Error, Result};
use crate::ytd::TextureInfo;

/// Default preview canvas size (px).
pub const DEFAULT_CANVAS: u32 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Webp,
    Png,
    Jpeg,
}

impl OutputFormat {
    pub fn ext(self) -> &'static str {
        match self {
            OutputFormat::Webp => "webp",
            OutputFormat::Png => "png",
            OutputFormat::Jpeg => "jpg",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "webp" => Some(OutputFormat::Webp),
            "png" => Some(OutputFormat::Png),
            "jpg" | "jpeg" => Some(OutputFormat::Jpeg),
            _ => None,
        }
    }
}

/// Decode a texture's largest mip to an RGBA image.
pub fn decode_texture(tex: &TextureInfo) -> Result<RgbaImage> {
    let dds_bytes = build_dds(tex)?;
    let dds = image_dds::ddsfile::Dds::read(dds_bytes.as_slice())
        .map_err(|e| Error::Parse(format!("dds read: {e}")))?;
    let img = image_dds::image_from_dds(&dds, 0)
        .map_err(|e| Error::Parse(format!("dds decode: {e}")))?;
    Ok(img)
}

/// Resize to a `size`×`size` canvas: square textures scale directly;
/// non-square ones scale to fit and center on a transparent canvas.
fn fit_square(img: RgbaImage, size: u32) -> RgbaImage {
    use image::imageops::FilterType;
    let (w, h) = (img.width(), img.height());
    if w == h {
        return image::imageops::resize(&img, size, size, FilterType::Lanczos3);
    }
    let thumb = DynamicImage::ImageRgba8(img)
        .resize(size, size, FilterType::Lanczos3)
        .to_rgba8();
    let (tw, th) = (thumb.width(), thumb.height());
    let mut canvas = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 0]));
    let x = ((size - tw) / 2) as i64;
    let y = ((size - th) / 2) as i64;
    image::imageops::overlay(&mut canvas, &thumb, x, y);
    canvas
}

/// Composite RGBA over a white background (for formats without alpha).
fn flatten_white(rgba: &RgbaImage) -> RgbImage {
    let mut out = RgbImage::from_pixel(rgba.width(), rgba.height(), Rgb([255, 255, 255]));
    for (x, y, p) in rgba.enumerate_pixels() {
        let a = p[3] as f32 / 255.0;
        let dst = out.get_pixel_mut(x, y);
        for i in 0..3 {
            dst[i] = (p[i] as f32 * a + dst[i] as f32 * (1.0 - a)).round() as u8;
        }
    }
    out
}

/// Decode + resize + encode a texture to preview-image bytes in `format`.
pub fn render_preview_bytes(
    tex: &TextureInfo,
    size: u32,
    format: OutputFormat,
) -> Result<Vec<u8>> {
    let square = fit_square(decode_texture(tex)?, size);

    match format {
        OutputFormat::Webp => {
            let enc = webp::Encoder::from_rgba(square.as_raw(), square.width(), square.height());
            Ok(enc.encode(90.0).to_vec())
        }
        OutputFormat::Png => {
            let mut buf = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(square)
                .write_to(&mut buf, ImageFormat::Png)
                .map_err(|e| Error::Parse(format!("png encode: {e}")))?;
            Ok(buf.into_inner())
        }
        OutputFormat::Jpeg => {
            let rgb = flatten_white(&square);
            let mut buf = Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(rgb)
                .write_to(&mut buf, ImageFormat::Jpeg)
                .map_err(|e| Error::Parse(format!("jpeg encode: {e}")))?;
            Ok(buf.into_inner())
        }
    }
}

/// Render a texture preview straight to a file.
pub fn save_preview(
    tex: &TextureInfo,
    out_path: &Path,
    size: u32,
    format: OutputFormat,
) -> Result<()> {
    let bytes = render_preview_bytes(tex, size, format)?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out_path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parse_and_ext() {
        assert_eq!(OutputFormat::parse("WEBP"), Some(OutputFormat::Webp));
        assert_eq!(OutputFormat::parse("jpeg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::parse("jpg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::parse("gif"), None);
        assert_eq!(OutputFormat::Jpeg.ext(), "jpg");
    }

    /// Full pure-Rust pipeline against a real file:
    /// .ytd -> RSC7 -> YTD -> diffuse -> DDS -> decode -> WebP/PNG.
    /// Set QENDERING_TEST_YTD to run; optionally QENDERING_TEST_OUT to dump
    /// the PNG for visual inspection. Skipped in CI.
    #[test]
    fn real_ytd_to_preview() {
        let Ok(path) = std::env::var("QENDERING_TEST_YTD") else {
            return;
        };
        let res = crate::rsc7::parse_file(Path::new(&path)).unwrap();
        let texs =
            crate::ytd::parse_texture_dictionary(&res.virtual_data, &res.physical_data).unwrap();
        let diff = crate::ytd::select_diffuse_texture(&texs).unwrap();

        let png = render_preview_bytes(diff, DEFAULT_CANVAS, OutputFormat::Png).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        let webp = render_preview_bytes(diff, DEFAULT_CANVAS, OutputFormat::Webp).unwrap();
        assert_eq!(&webp[0..4], b"RIFF");
        eprintln!("preview: png={} bytes, webp={} bytes", png.len(), webp.len());

        if let Ok(out) = std::env::var("QENDERING_TEST_OUT") {
            std::fs::write(out, &png).unwrap();
        }
    }
}
