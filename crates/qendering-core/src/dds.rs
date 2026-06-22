//! Build a complete DDS file from a parsed [`TextureInfo`].
//!
//! GTA textures arrive as raw pixel bytes with no DDS header; this wraps them
//! in a valid `DDS ` container so a standard DDS decoder can read them. BC7
//! uses the DX10 extended header; other block formats use the classic FourCC.

use crate::error::{Error, Result};
use crate::ytd::{block_bytes, TextureInfo};

const DDS_MAGIC: &[u8; 4] = b"DDS ";

// DDS_HEADER dwFlags: CAPS|HEIGHT|WIDTH|PIXELFORMAT|MIPMAPCOUNT|LINEARSIZE
const HEADER_FLAGS: u32 = 0x000A_1007;
// dwCaps: TEXTURE|MIPMAP|COMPLEX
const HEADER_CAPS: u32 = 0x0040_1008;

// DDS_PIXELFORMAT dwFlags
const DDPF_ALPHAPIXELS: u32 = 0x1;
const DDPF_FOURCC: u32 = 0x4;
const DDPF_RGB: u32 = 0x40;
const DDPF_LUMINANCE: u32 = 0x2_0000;

const FOURCC_DXT1: u32 = 0x3154_5844;
const FOURCC_DXT3: u32 = 0x3354_5844;
const FOURCC_DXT5: u32 = 0x3554_5844;
const FOURCC_ATI1: u32 = 0x3149_5441;
const FOURCC_ATI2: u32 = 0x3249_5441;
const FOURCC_DX10: u32 = 0x3031_5844;

fn fourcc_for(format_name: &str) -> Option<u32> {
    Some(match format_name {
        "DXT1" => FOURCC_DXT1,
        "DXT3" => FOURCC_DXT3,
        "DXT5" => FOURCC_DXT5,
        "ATI1" => FOURCC_ATI1,
        "ATI2" => FOURCC_ATI2,
        _ => return None,
    })
}

fn mip0_size(width: usize, height: usize, format_name: &str) -> Result<usize> {
    if let Some(bs) = block_bytes(format_name) {
        let bx = ((width + 3) / 4).max(1);
        let by = ((height + 3) / 4).max(1);
        return Ok(bx * by * bs);
    }
    Ok(match format_name {
        "A8R8G8B8" | "X8R8G8B8" | "A8B8G8R8" => width * height * 4,
        "A1R5G5B5" => width * height * 2,
        "L8" | "A8" => width * height,
        _ => {
            return Err(Error::Parse(format!(
                "cannot compute mip0 size for format {format_name}"
            )))
        }
    })
}

/// Build the 32-byte DDS_PIXELFORMAT block.
fn pixelformat(format_name: &str) -> Result<[u8; 32]> {
    let mut pf = [0u8; 32];
    pf[0..4].copy_from_slice(&32u32.to_le_bytes()); // dwSize

    let put_fourcc = |pf: &mut [u8; 32], cc: u32| {
        pf[4..8].copy_from_slice(&DDPF_FOURCC.to_le_bytes());
        pf[8..12].copy_from_slice(&cc.to_le_bytes());
    };

    if let Some(cc) = fourcc_for(format_name) {
        put_fourcc(&mut pf, cc);
    } else if format_name == "BC7" {
        put_fourcc(&mut pf, FOURCC_DX10);
    } else if matches!(format_name, "A8R8G8B8" | "X8R8G8B8" | "A8B8G8R8") {
        pf[4..8].copy_from_slice(&(DDPF_RGB | DDPF_ALPHAPIXELS).to_le_bytes());
        pf[12..16].copy_from_slice(&32u32.to_le_bytes()); // dwRGBBitCount
        pf[16..20].copy_from_slice(&0x00FF_0000u32.to_le_bytes()); // R
        pf[20..24].copy_from_slice(&0x0000_FF00u32.to_le_bytes()); // G
        pf[24..28].copy_from_slice(&0x0000_00FFu32.to_le_bytes()); // B
        pf[28..32].copy_from_slice(&0xFF00_0000u32.to_le_bytes()); // A
    } else if matches!(format_name, "L8" | "A8") {
        // A8 shares L8's layout; emit as luminance for decoder compatibility.
        pf[4..8].copy_from_slice(&DDPF_LUMINANCE.to_le_bytes());
        pf[12..16].copy_from_slice(&8u32.to_le_bytes());
        pf[16..20].copy_from_slice(&0xFFu32.to_le_bytes());
    } else {
        return Err(Error::Parse(format!(
            "unsupported DDS pixel format: {format_name}"
        )));
    }
    Ok(pf)
}

fn dx10_header() -> [u8; 20] {
    let mut h = [0u8; 20];
    h[0..4].copy_from_slice(&98u32.to_le_bytes()); // DXGI_FORMAT_BC7_UNORM
    h[4..8].copy_from_slice(&3u32.to_le_bytes()); // TEXTURE2D
    h[12..16].copy_from_slice(&1u32.to_le_bytes()); // arraySize
    h
}

/// Construct a complete DDS file (magic + header [+ DX10] + pixel data).
pub fn build_dds(texture: &TextureInfo) -> Result<Vec<u8>> {
    let fmt = texture.format_name.as_str();
    let w = texture.width as usize;
    let h = texture.height as usize;
    let mips = (texture.mip_levels as u32).max(1);

    let linear_size = mip0_size(w, h, fmt)? as u32;
    let pf = pixelformat(fmt)?;

    let mut header = Vec::with_capacity(124);
    header.extend_from_slice(&124u32.to_le_bytes()); // dwSize
    header.extend_from_slice(&HEADER_FLAGS.to_le_bytes());
    header.extend_from_slice(&(texture.height as u32).to_le_bytes());
    header.extend_from_slice(&(texture.width as u32).to_le_bytes());
    header.extend_from_slice(&linear_size.to_le_bytes()); // dwPitchOrLinearSize
    header.extend_from_slice(&0u32.to_le_bytes()); // dwDepth
    header.extend_from_slice(&mips.to_le_bytes()); // dwMipMapCount
    header.extend_from_slice(&[0u8; 44]); // dwReserved1[11]
    header.extend_from_slice(&pf); // ddspf (32)
    header.extend_from_slice(&HEADER_CAPS.to_le_bytes());
    header.extend_from_slice(&[0u8; 16]); // caps2..4 + reserved2
    debug_assert_eq!(header.len(), 124);

    let mut out = Vec::with_capacity(4 + 124 + 20 + texture.raw_data.len());
    out.extend_from_slice(DDS_MAGIC);
    out.extend_from_slice(&header);
    if fmt == "BC7" {
        out.extend_from_slice(&dx10_header());
    }
    out.extend_from_slice(&texture.raw_data);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ytd::TextureInfo;

    fn tex(format: &str, raw: usize) -> TextureInfo {
        TextureInfo {
            name: "t".into(),
            width: 256,
            height: 256,
            format_code: 0,
            format_name: format.into(),
            mip_levels: 1,
            stride: 0,
            raw_data: vec![0u8; raw],
        }
    }

    #[test]
    fn dxt1_header_layout() {
        let dds = build_dds(&tex("DXT1", 32768)).unwrap();
        assert_eq!(&dds[0..4], b"DDS ");
        // magic(4) + header(124) + data(32768), no DX10 chunk
        assert_eq!(dds.len(), 4 + 124 + 32768);
        // dwSize at offset 4 == 124
        assert_eq!(u32::from_le_bytes(dds[4..8].try_into().unwrap()), 124);
        // FourCC at file offset 0x54 (magic 4 + ddspf@72 + dwFourCC@8) is "DXT1"
        assert_eq!(&dds[0x54..0x58], b"DXT1");
    }

    #[test]
    fn bc7_uses_dx10_header() {
        let dds = build_dds(&tex("BC7", 65536)).unwrap();
        // magic + header + DX10(20) + data
        assert_eq!(dds.len(), 4 + 124 + 20 + 65536);
        assert_eq!(&dds[0x54..0x58], b"DX10");
    }

    #[test]
    fn unsupported_format_errors() {
        assert!(build_dds(&tex("UNKNOWN(0x1)", 0)).is_err());
    }
}
