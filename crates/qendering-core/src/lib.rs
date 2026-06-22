//! Core logic for Qendering.
//!
//! Pure-Rust port of the asset-handling pieces: parsing structured GTA V
//! filenames, discovering clothing/object assets, decoding textures, and
//! encoding preview images. Everything here is independent of the UI and of
//! Blender, so it can be unit-tested in isolation.

pub mod discovery;
pub mod error;
pub mod filename;
pub mod rsc7;

pub use error::{Error, Result};
