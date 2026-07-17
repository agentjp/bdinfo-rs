//! bdinfo-rs desktop GUI — the library half.
//!
//! Holds the Tier-A logic — the playlist view-model ([`model`]), the
//! master-detail pane view-models ([`panes`]), the selection model (`selection`),
//! the live-progress model ([`progress`]), the flow state machine ([`flow`]),
//! the copy-path helpers ([`paths`]), the clipboard sanitizer ([`clipboard`]),
//! the column-weight math ([`columns`]), the persistent configuration
//! ([`settings`]), the best-effort diagnostics log ([`diagnostics`]), the
//! visual identity ([`theme`]) and window-icon rendering
//! ([`icon`]), and the scan seam ([`scan`]) — so the golden-tie + unit
//! tests link them directly, and the `bdinfo-rs-gui` binary is the thin iced
//! (Tier-B) shell over this surface: it translates messages to calls here and
//! results to widgets, holding no business logic of its own. The one impure thing
//! the shell does is spawn the measured-scan worker thread and forward its
//! progress over a channel.
//!
//! This crate is an INDEPENDENT workspace (see `Cargo.toml`) — the large
//! iced/wgpu/winit tree stays out of the root gate — but it STAYS inside
//! `forbid(unsafe_code)`: a native iced app needs no `unsafe` in our own code.
#![forbid(unsafe_code)]

pub mod clipboard;
pub mod columns;
pub mod diagnostics;
pub mod flow;
pub mod icon;
pub mod model;
pub mod panes;
pub mod paths;
pub mod progress;
pub mod scan;
pub mod settings;
pub mod theme;
// Private: the binary drives selection only through `flow`, never this module
// directly (so public-API docs reference it as a plain code span, not a link).
mod selection;
