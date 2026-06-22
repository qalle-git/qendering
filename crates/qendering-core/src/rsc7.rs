//! RSC7 container parser.
//!
//! A `.ytd` (and `.ydd`/`.ydr`) file is an RSC7 resource: a 16-byte header
//! followed by a raw-DEFLATE payload that decompresses into a *virtual*
//! segment (structs/headers) and a *physical* segment (pixel data).
//!
//! Header (16 bytes, little-endian):
//! | off | field          | meaning                          |
//! |-----|----------------|----------------------------------|
//! | 0x0 | magic          | `0x37435352` ("RSC7")            |
//! | 0x4 | version        | 13 = PC/legacy                   |
//! | 0x8 | system_flags   | encodes virtual segment size     |
//! | 0xC | graphics_flags | encodes physical segment size    |

use std::io::Read;
use std::path::Path;

use flate2::read::DeflateDecoder;

use crate::error::{Error, Result};

pub const RSC7_MAGIC: u32 = 0x3743_5352;
const HEADER_SIZE: usize = 16;
const MIN_FILE_SIZE: usize = 32;
pub const EXPECTED_VERSION: u32 = 13;

/// A decompressed RSC7 resource.
#[derive(Debug, Clone)]
pub struct Rsc7Resource {
    pub version: u32,
    /// Struct data (texture dictionary, texture headers).
    pub virtual_data: Vec<u8>,
    /// Raw pixel data.
    pub physical_data: Vec<u8>,
}

/// Decode a decompressed segment size from an RSC7 flag field.
///
/// Ported from CodeWalker's `RpfFile`: each flag bit-field encodes a count of
/// pages at a size multiple of a base page size.
pub fn size_from_flags(flags: u32) -> usize {
    let s0 = ((flags >> 27) & 0x1) << 0;
    let s1 = ((flags >> 26) & 0x1) << 1;
    let s2 = ((flags >> 25) & 0x1) << 2;
    let s3 = ((flags >> 24) & 0x1) << 3;
    let s4 = ((flags >> 17) & 0x7F) << 4;
    let s5 = ((flags >> 11) & 0x3F) << 5;
    let s6 = ((flags >> 7) & 0xF) << 6;
    let s7 = ((flags >> 5) & 0x3) << 7;
    let s8 = ((flags >> 4) & 0x1) << 8;
    let ss = flags & 0xF;
    let base_size: usize = 0x200usize << ss;
    let total = (s0 + s1 + s2 + s3 + s4 + s5 + s6 + s7 + s8) as usize;
    base_size * total
}

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

/// Parse RSC7 bytes into decompressed virtual/physical segments.
pub fn parse_bytes(raw: &[u8]) -> Result<Rsc7Resource> {
    if raw.len() < MIN_FILE_SIZE {
        return Err(Error::TooSmall(raw.len()));
    }

    let magic = read_u32_le(raw, 0);
    if magic != RSC7_MAGIC {
        return Err(Error::BadMagic(magic));
    }
    let version = read_u32_le(raw, 4);
    let system_flags = read_u32_le(raw, 8);
    let graphics_flags = read_u32_le(raw, 12);

    let virtual_size = size_from_flags(system_flags);
    let physical_size = size_from_flags(graphics_flags);

    // Raw DEFLATE (no zlib/gzip wrapper) — same as Python's zlib.decompress(data, -15).
    let mut decompressed = Vec::new();
    DeflateDecoder::new(&raw[HEADER_SIZE..])
        .read_to_end(&mut decompressed)
        .map_err(|e| Error::Decompress(e.to_string()))?;

    let total = virtual_size + physical_size;
    if total > decompressed.len() {
        return Err(Error::SegmentOverflow {
            total,
            actual: decompressed.len(),
        });
    }

    let virtual_data = decompressed[..virtual_size].to_vec();
    let physical_data = decompressed[virtual_size..total].to_vec();

    Ok(Rsc7Resource {
        version,
        virtual_data,
        physical_data,
    })
}

/// Read and parse an RSC7 file from disk.
pub fn parse_file(path: &Path) -> Result<Rsc7Resource> {
    let raw = std::fs::read(path)?;
    parse_bytes(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    #[test]
    fn flag_size_math() {
        assert_eq!(size_from_flags(0), 0);
        // s0 bit set, ss=0 -> base 0x200 * 1 page = 512.
        assert_eq!(size_from_flags(1 << 27), 512);
        // s0 set, ss=1 -> base (0x200<<1)=1024 * 1 = 1024.
        assert_eq!(size_from_flags((1 << 27) | 1), 1024);
        // s1 set (counts as 2 pages), ss=0 -> 512 * 2 = 1024.
        assert_eq!(size_from_flags(1 << 26), 1024);
    }

    fn raw_deflate(data: &[u8]) -> Vec<u8> {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn round_trip_split() {
        // Choose flags that decode to 512 + 512.
        let system_flags: u32 = 1 << 27; // 512
        let graphics_flags: u32 = 1 << 27; // 512
        assert_eq!(size_from_flags(system_flags), 512);

        let mut payload = vec![0u8; 1024];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let compressed = raw_deflate(&payload);

        let mut raw = Vec::new();
        raw.extend_from_slice(&RSC7_MAGIC.to_le_bytes());
        raw.extend_from_slice(&13u32.to_le_bytes());
        raw.extend_from_slice(&system_flags.to_le_bytes());
        raw.extend_from_slice(&graphics_flags.to_le_bytes());
        raw.extend_from_slice(&compressed);

        let res = parse_bytes(&raw).unwrap();
        assert_eq!(res.version, 13);
        assert_eq!(res.virtual_data.len(), 512);
        assert_eq!(res.physical_data.len(), 512);
        assert_eq!(res.virtual_data, &payload[..512]);
        assert_eq!(res.physical_data, &payload[512..]);
    }

    #[test]
    fn rejects_bad_magic() {
        let raw = vec![0u8; 64];
        assert!(matches!(parse_bytes(&raw), Err(Error::BadMagic(_))));
    }

    #[test]
    fn rejects_too_small() {
        let raw = vec![0u8; 8];
        assert!(matches!(parse_bytes(&raw), Err(Error::TooSmall(8))));
    }

    /// Real-file smoke test. Set QENDERING_TEST_YTD to a real .ytd path to run;
    /// skipped in CI (env unset).
    #[test]
    fn real_ytd_smoke() {
        let Ok(path) = std::env::var("QENDERING_TEST_YTD") else {
            return;
        };
        let res = parse_file(Path::new(&path)).expect("parse real .ytd");
        assert!(
            !res.virtual_data.is_empty(),
            "virtual segment should be non-empty"
        );
        eprintln!(
            "real ytd: version={} virtual={} physical={}",
            res.version,
            res.virtual_data.len(),
            res.physical_data.len()
        );
    }
}
