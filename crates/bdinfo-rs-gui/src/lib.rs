//! bdinfo-rs desktop GUI — the library half.
//!
//! Holds the Tier-A playlist view-model ([`model`]) and the scan seam
//! ([`scan`]) so the golden-tie + unit tests link them directly, and the
//! `bdinfo-rs-gui` binary is the thin iced (Tier-B) shell over this surface —
//! it translates messages to calls here and results to widgets, holding no
//! business logic of its own.
//!
//! This crate is an INDEPENDENT workspace (see `Cargo.toml`) — the large
//! iced/wgpu/winit tree stays out of the root gate — but it STAYS inside
//! `forbid(unsafe_code)`: a native iced app needs no `unsafe` in our own code.
#![forbid(unsafe_code)]

pub mod model;
pub mod scan;
