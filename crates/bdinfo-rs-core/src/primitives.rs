//! Domain newtypes for the Blu-ray model's primitive values.
//!
//! An elementary stream's packet identifier travels the codebase as a 13-bit
//! value in a `u16`, so any bare `u16` could be passed where a PID was
//! meant. This module pins that intent at the type level (Rust API guideline
//! C-NEWTYPE): [`Pid`] wraps a `u16` so unit/role confusion is a compile error,
//! with zero runtime cost (it is `#[repr(transparent)]`-equivalent — a one-field
//! tuple struct the compiler lays out exactly as its `u16`).
//!
//! **Where the newtype applies.** `Pid` is the *identity of an elementary stream*
//! wherever the model stores or surfaces it — [`crate::stream::TsStreamBase::pid`], the
//! [`pid`](crate::stream::TsStream::pid) accessor, and the report's
//! [`StreamSummary`](crate::bdrom::disc::StreamSummary). Raw `u16` is kept for the
//! *transport-stream wire value* during demux: the M2TS parser assembles a PID one
//! bit-field at a time (`(byte & 0x1F) << 8 | next`), and the demux keys its
//! working `BTreeMap`s by that wire value. A `Pid` is minted at the demux's
//! stream-registration boundary and read back out as a
//! `u16` via [`Pid::get`] only where the wire value is needed again. Keeping the
//! wrapping math on `u16` honours the rule that a newtype *wraps* a value without
//! *widening* its behaviour — `Pid` exposes no arithmetic.

use core::fmt;

/// An elementary stream's packet identifier (`PID`) — a distinct type so it
/// cannot be confused with any other `u16`.
///
/// Construct with [`Pid::new`] and read the raw value with [`Pid::get`]. `Pid`
/// orders and displays exactly as its inner `u16` (so sorting streams by PID and
/// the report's zero-padded `stream.NNNNN` formatting are unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pid(u16);

impl Pid {
    /// Wraps a raw 13-bit transport PID value.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Returns the raw `u16` PID — used where the wire value is needed (demux map
    /// keys, codec PID thresholds).
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for Pid {
    /// Forwards to the inner `u16`'s `Display`, so format flags (e.g. the report's
    /// `{:05}` zero-padding) apply to the PID value verbatim.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::Pid;

    #[test]
    fn new_and_get_round_trip() {
        assert_eq!(Pid::new(0x1011).get(), 0x1011);
        assert_eq!(Pid::new(0).get(), 0);
        assert_eq!(Pid::new(u16::MAX).get(), u16::MAX);
    }

    #[test]
    fn default_is_pid_zero() {
        assert_eq!(Pid::default(), Pid::new(0));
        assert_eq!(Pid::default().get(), 0);
    }

    #[test]
    fn display_forwards_to_the_inner_u16_with_format_flags() {
        // Plain and zero-padded — the report emits `{:05}`.
        assert_eq!(Pid::new(4113).to_string(), "4113");
        assert_eq!(format!("{:05}", Pid::new(4113)), "04113");
        assert_eq!(format!("{}", Pid::new(0)), "0");
    }

    #[test]
    fn orders_and_compares_like_the_inner_value() {
        assert!(Pid::new(0x1011) < Pid::new(0x1100));
        assert_eq!(Pid::new(0x1100).cmp(&Pid::new(0x1011)), core::cmp::Ordering::Greater);
        assert_ne!(Pid::new(1), Pid::new(2));
        // Debug + Copy are exercised here too.
        let copied = Pid::new(7);
        assert_eq!(format!("{copied:?}"), "Pid(7)");
        assert_eq!(copied, Pid::new(7));
    }
}
