//! Screen capture implementation for Windows using the `windows` crate
//! (`Windows.Graphics.Capture` and WASAPI).

mod capture;
mod utils;

pub use capture::*;
