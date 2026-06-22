//! YTD texture-dictionary parser.
//!
//! Parses the `TextureDictionary` struct out of a decompressed RSC7 resource
//! and extracts each texture's metadata + raw pixel bytes.
//!
//! GTA V resources use two virtual address spaces: pointers `>= 0x60000000`
//! reference the physical (pixel) segment, `>= 0x50000000` the virtual
//! (struct) segment. Only the low 32 bits of each 64-bit pointer are used.

use crate::error::{Error, Result};

const VIRTUAL_BASE: u32 = 0x5000_0000;
const PHYSICAL_BASE: u32 = 0x6000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    Null,
    Virtual,
    Physical,
}

/// Resolve a resource pointer to `(segment, offset)`.
pub fn resolve_pointer(ptr: u32) -> Result<(Segment, usize)> {
    if ptr == 0 {
        Ok((Segment::Null, 0))
    } else if ptr >= PHYSICAL_BASE {
        Ok((Segment::Physical, (ptr - PHYSICAL_BASE) as usize))
    } else if ptr >= VIRTUAL_BASE {
        Ok((Segment::Virtual, (ptr - VIRTUAL_BASE) as usize))
    } else {
        Err(Error::Parse(format!("unknown pointer base: 0x{ptr:08X}")))
    }
}

/// `(format_name, bits_per_pixel)` for a GTA texture format code.
pub fn format_for_code(code: u32) -> Option<(&'static str, u32)> {
    Some(match code {
        21 => ("A8R8G8B8", 32),
        22 => ("X8R8G8B8", 32),
        25 => ("A1R5G5B5", 16),
        28 => ("A8", 8),
        32 => ("A8B8G8R8", 32),
        50 => ("L8", 8),
        0x3154_5844 => ("DXT1", 4),
        0x3354_5844 => ("DXT3", 8),
        0x3554_5844 => ("DXT5", 8),
        0x3149_5441 => ("ATI1", 4),
        0x3249_5441 => ("ATI2", 8),
        0x2037_4342 => ("BC7", 8),
        _ => return None,
    })
}

/// Bytes per 4x4 block for a block-compressed format, if any.
pub fn block_bytes(format_name: &str) -> Option<usize> {
    Some(match format_name {
        "DXT1" | "ATI1" => 8,
        "DXT3" | "DXT5" | "ATI2" | "BC7" => 16,
        _ => return None,
    })
}

fn calc_mip_size(width: usize, height: usize, format_name: &str, bpp: u32) -> usize {
    if let Some(bs) = block_bytes(format_name) {
        let bx = ((width + 3) / 4).max(1);
        let by = ((height + 3) / 4).max(1);
        bx * by * bs
    } else {
        width * height * (bpp as usize / 8)
    }
}

fn calc_total_data_size(
    width: usize,
    height: usize,
    mip_levels: u32,
    format_name: &str,
    bpp: u32,
) -> usize {
    let mut total = 0;
    let (mut w, mut h) = (width, height);
    for _ in 0..mip_levels {
        total += calc_mip_size(w, h, format_name, bpp);
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    total
}

/// A parsed texture from a texture dictionary.
#[derive(Debug, Clone)]
pub struct TextureInfo {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub format_code: u32,
    pub format_name: String,
    pub mip_levels: u8,
    pub stride: u16,
    pub raw_data: Vec<u8>,
}

fn ru16(d: &[u8], o: usize) -> Option<u16> {
    d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}
fn ru32(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn ru64_low32(d: &[u8], o: usize) -> Option<u32> {
    // Only the low 32 bits of the 64-bit pointer are meaningful.
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_cstr(data: &[u8], offset: usize, max_len: usize) -> String {
    let end = (offset + max_len).min(data.len());
    let slice = &data[offset.min(data.len())..end];
    let n = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..n]).into_owned()
}

/// Parse all textures from a decompressed RSC7 resource.
pub fn parse_texture_dictionary(
    virtual_data: &[u8],
    physical_data: &[u8],
) -> Result<Vec<TextureInfo>> {
    if virtual_data.len() < 64 {
        return Err(Error::Parse(format!(
            "virtual data too small for TextureDictionary header ({} bytes)",
            virtual_data.len()
        )));
    }

    let textures_ptr = ru64_low32(virtual_data, 0x30)
        .ok_or_else(|| Error::Parse("missing textures pointer".into()))?;
    let textures_count =
        ru16(virtual_data, 0x38).ok_or_else(|| Error::Parse("missing texture count".into()))?;

    if textures_count == 0 {
        return Ok(Vec::new());
    }

    let (seg, arr_offset) = resolve_pointer(textures_ptr)?;
    if seg != Segment::Virtual {
        return Err(Error::Parse(
            "textures pointer does not resolve to the virtual segment".into(),
        ));
    }

    // Array of 64-bit pointers (low 32 bits each), one per texture.
    let mut tex_pointers = Vec::with_capacity(textures_count as usize);
    for i in 0..textures_count as usize {
        match ru64_low32(virtual_data, arr_offset + i * 8) {
            Some(p) => tex_pointers.push(p),
            None => break,
        }
    }

    let mut results = Vec::new();
    for tptr in tex_pointers {
        let (seg, tex_offset) = match resolve_pointer(tptr) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if seg != Segment::Virtual || tex_offset + 144 > virtual_data.len() {
            continue;
        }

        // Name
        let name = match ru64_low32(virtual_data, tex_offset + 0x28) {
            Some(np) if np != 0 => match resolve_pointer(np) {
                Ok((Segment::Virtual, noff)) if noff < virtual_data.len() => {
                    read_cstr(virtual_data, noff, 256)
                }
                _ => String::new(),
            },
            _ => String::new(),
        };

        let width = ru16(virtual_data, tex_offset + 0x50).unwrap_or(0);
        let height = ru16(virtual_data, tex_offset + 0x52).unwrap_or(0);
        let stride = ru16(virtual_data, tex_offset + 0x56).unwrap_or(0);
        let format_code = ru32(virtual_data, tex_offset + 0x58).unwrap_or(0);
        let mip_levels = *virtual_data.get(tex_offset + 0x5D).unwrap_or(&1);

        let (format_name, bpp) = match format_for_code(format_code) {
            Some((n, b)) => (n.to_string(), b),
            None => (format!("UNKNOWN(0x{format_code:X})"), 0),
        };

        // Pixel data
        let mut raw_data = Vec::new();
        if bpp > 0 {
            if let Some(dptr) = ru64_low32(virtual_data, tex_offset + 0x70) {
                if dptr != 0 {
                    if let Ok((Segment::Physical, doff)) = resolve_pointer(dptr) {
                        let want = calc_total_data_size(
                            width as usize,
                            height as usize,
                            (mip_levels as u32).max(1),
                            &format_name,
                            bpp,
                        );
                        let available = physical_data.len().saturating_sub(doff);
                        let take = want.min(available);
                        if take > 0 {
                            raw_data = physical_data[doff..doff + take].to_vec();
                        }
                    }
                }
            }
        }

        results.push(TextureInfo {
            name,
            width,
            height,
            format_code,
            format_name,
            mip_levels,
            stride,
            raw_data,
        });
    }

    Ok(results)
}

const NON_DIFFUSE_SUFFIXES: [&str; 3] = ["_n", "_s", "_m"];

/// Pick the diffuse texture: single texture wins outright; otherwise exclude
/// `_n`/`_s`/`_m` channels and take the highest-resolution remaining one.
pub fn select_diffuse_texture(textures: &[TextureInfo]) -> Option<&TextureInfo> {
    if textures.is_empty() {
        return None;
    }
    if textures.len() == 1 {
        return Some(&textures[0]);
    }
    textures
        .iter()
        .filter(|t| {
            let lower = t.name.to_lowercase();
            !NON_DIFFUSE_SUFFIXES.iter().any(|s| lower.ends_with(s))
        })
        .max_by_key(|t| t.width as u32 * t.height as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_resolution() {
        assert_eq!(resolve_pointer(0).unwrap(), (Segment::Null, 0));
        assert_eq!(
            resolve_pointer(0x5000_0010).unwrap(),
            (Segment::Virtual, 0x10)
        );
        assert_eq!(
            resolve_pointer(0x6000_0040).unwrap(),
            (Segment::Physical, 0x40)
        );
        assert!(resolve_pointer(0x1234).is_err());
    }

    #[test]
    fn formats_and_mip_math() {
        assert_eq!(format_for_code(0x3154_5844), Some(("DXT1", 4)));
        assert_eq!(format_for_code(0x2037_4342), Some(("BC7", 8)));
        assert_eq!(format_for_code(0xDEAD), None);
        // 256x256 DXT1: 64x64 blocks * 8 bytes = 32768.
        assert_eq!(calc_mip_size(256, 256, "DXT1", 4), 32768);
        // 256x256 A8R8G8B8: 256*256*4 = 262144.
        assert_eq!(calc_mip_size(256, 256, "A8R8G8B8", 32), 262144);
    }

    #[test]
    fn diffuse_selection() {
        let mk = |name: &str, w: u16, h: u16| TextureInfo {
            name: name.into(),
            width: w,
            height: h,
            format_code: 0,
            format_name: "DXT1".into(),
            mip_levels: 1,
            stride: 0,
            raw_data: vec![],
        };
        let texs = vec![
            mk("shirt_diff", 512, 512),
            mk("shirt_n", 1024, 1024), // normal — excluded despite bigger
            mk("shirt_s", 256, 256),
        ];
        let chosen = select_diffuse_texture(&texs).unwrap();
        assert_eq!(chosen.name, "shirt_diff");
    }

    /// End-to-end against a real file (RSC7 -> YTD -> diffuse -> DDS).
    /// Set QENDERING_TEST_YTD to run; skipped in CI.
    #[test]
    fn real_ytd_to_dds() {
        let Ok(path) = std::env::var("QENDERING_TEST_YTD") else {
            return;
        };
        let res = crate::rsc7::parse_file(std::path::Path::new(&path)).unwrap();
        let texs = parse_texture_dictionary(&res.virtual_data, &res.physical_data).unwrap();
        assert!(!texs.is_empty(), "expected at least one texture");
        let diff = select_diffuse_texture(&texs).expect("a diffuse texture");
        eprintln!(
            "diffuse: name='{}' {}x{} {} mips={} raw={} bytes",
            diff.name,
            diff.width,
            diff.height,
            diff.format_name,
            diff.mip_levels,
            diff.raw_data.len()
        );
        assert!(diff.width > 0 && diff.height > 0);
        assert!(!diff.raw_data.is_empty());

        let dds = crate::dds::build_dds(diff).unwrap();
        assert_eq!(&dds[0..4], b"DDS ");
        assert!(dds.len() > 128);
        eprintln!("built DDS: {} bytes", dds.len());
    }
}
