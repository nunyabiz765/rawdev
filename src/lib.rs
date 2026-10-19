//! rawdev - a RAW develop engine built for scripts and AI agents.
//!
//! LibRaw's `dcraw_emu` decodes the RAW; everything after that (tone curve, noise reduction, colour,
//! fringe cleanup, sharpening, resizing) is in this crate, multithreaded, with an optional bit-exact GPU
//! path for the noise reduction. Looks are TOML files (`looks/`). The command line (`src/main.rs`) adds
//! JSON output, dry runs, manifests and the write guards in `safety.rs`. See README.md for use and
//! docs/DEVELOPING.md for a tour of the code.

// Index loops (`for i in 0..n`) are often the clearest way to write pixel maths, so we allow them.
#![allow(clippy::needless_range_loop)]

pub mod day;
pub mod decode;
pub mod develop;
pub mod export;
pub mod gpu;
pub mod image;
pub mod ingest;
pub mod look;
pub mod lookfile;
pub mod manifest;
pub mod nlm;
pub mod opencv;
pub mod pillow;
pub mod recipe;
pub mod reference;
pub mod safety;
pub mod tone;
pub mod tools;

/// The error type used everywhere: "any error", boxed. Simple, and `?` works on all of them.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
