//! BHCapture core — pure, portable logic.
//!
//! Deliberately has **no** dependency on Tauri, on any UI toolkit, or on platform discovery.
//! The shell injects every path and every display it finds; this crate only does arithmetic,
//! which is what makes the hard parts testable without a screen attached.

pub mod denoise;
pub mod export;
pub mod geom;
pub mod hotkey;
pub mod image;
pub mod level;
pub mod naming;
pub mod record;
pub mod settings;
pub mod snap;
