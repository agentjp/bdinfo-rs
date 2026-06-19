//! Disc report composition.
//!
//! The library composes reports as `String`s — printing or saving them is the
//! caller's job (the library never writes to stdout). The one report format is
//! the classic human-readable disc report in [`text`].

pub mod text;
