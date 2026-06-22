//! Crate-wide error type.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not an RSC7 file: bad magic 0x{0:08X}")]
    BadMagic(u32),

    #[error("file too small: {0} bytes")]
    TooSmall(usize),

    #[error("decompression failed: {0}")]
    Decompress(String),

    #[error("segment sizes ({total}) exceed decompressed length ({actual})")]
    SegmentOverflow { total: usize, actual: usize },

    #[error("parse error: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, Error>;
