//! BHCapture core — pure, portable logic.
//!
//! Deliberately has **no** dependency on Tauri, on any UI toolkit, or on platform discovery.
//! The shell injects every path and every display it finds; this crate only does arithmetic,
//! which is what makes the hard parts testable without a screen attached.

pub mod geom;
pub mod image;
pub mod naming;
pub mod settings;
