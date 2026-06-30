//! bdinfo-rs desktop GUI — the library half.
//!
//! Holds the Tier-A logic — the playlist view-model ([`model`]), the selection
//! model (`selection`), the live-progress model ([`progress`]), and the scan
//! seam ([`scan`]) — so the golden-tie + unit tests link them directly, and the
//! `bdinfo-rs-gui` binary is the thin iced (Tier-B) shell over this surface: it
//! translates messages to calls here and results to widgets, holding no business
//! logic of its own.
//!
//! This crate is an INDEPENDENT workspace (see `Cargo.toml`) — the large
//! iced/wgpu/winit tree stays out of the root gate — but it STAYS inside
//! `forbid(unsafe_code)`: a native iced app needs no `unsafe` in our own code.
#![forbid(unsafe_code)]

pub mod model;
pub mod progress;
pub mod scan;
// Private: the binary drives selection only through the flow state machine,
// never this module directly (so public-API docs reference it as a plain code
// span, not a link).
mod selection;
