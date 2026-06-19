//! Bit-level reader over an in-memory transport-stream payload — the MSB-first
//! bitstream primitive every codec parser drives.
//!
//! [`TsStreamBuffer`] is an appended byte buffer plus a bit cursor
//! (`skip_bits`) and an emulation-prevention skip counter (`skipped_bytes`),
//! with big-endian, most-significant-bit-first reads.
//!
//! Design contract: the buffer presents a fixed 5 MiB logical window, with the
//! backing storage grown lazily — a read past the stored content but inside
//! the window yields a zero byte, and a read at or past the window yields
//! `0xFF` (the EOF sentinel). The emulation-prevention look-back is skipped
//! when fewer than two bytes precede the cursor; bit positions beyond the
//! 64-bit [`read_bits8`](TsStreamBuffer::read_bits8) window contribute zero;
//! and Exp-Golomb decoding is bounded, using integer `1 << n` arithmetic.
//! Hostile input can therefore never panic, hang, or read out of bounds.
//!
//! All fixed-width bit math uses `wrapping_*` shifts and accumulation; buffer
//! offsets use checked/saturating arithmetic so a malformed length can never
//! panic or read out of bounds.

/// Fixed capacity of the backing byte buffer — the 5 MiB logical window.
/// [`add`]
/// clamps so the stored length never exceeds this; a read past the stored
/// content but within this size yields `0x00` (the window is zero-filled),
/// and a read at or past it yields `0xFF` (the EOF sentinel).
///
/// [`add`]: TsStreamBuffer::add
const BUFFER_SIZE: usize = 5_242_880;

/// Reference point for [`TsStreamBuffer::seek`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekOrigin {
    /// Relative to the start of the buffer.
    Begin,
    /// Relative to the current position.
    Current,
    /// Relative to the end of the underlying 5 MiB stream.
    End,
}

/// An MSB-first bit reader over an appended byte buffer.
///
/// Bytes are appended with [`add`](Self::add); [`begin_read`](Self::begin_read)
/// rewinds to the start, then [`read_bool`](Self::read_bool) /
/// [`read_bits2`](Self::read_bits2) / [`read_bits4`](Self::read_bits4) /
/// [`read_bits8`](Self::read_bits8) consume bits most-significant-first, and the
/// `bs_skip_*` / Exp-Golomb helpers build on them. Every read is bounds-checked:
/// a read past the stored content yields zero bytes rather than panicking.
#[derive(Debug, Default)]
pub struct TsStreamBuffer {
    /// Appended content. Grown lazily, but
    /// capped at [`BUFFER_SIZE`] so the observable length never exceeds the
    /// 5 MiB logical window.
    buffer: Vec<u8>,
    /// Read cursor in bytes.
    position: usize,
    /// Bits already consumed within the current byte, MSB-first.
    /// Held in `0..=7` between reads (reduced modulo 8). Typed
    /// `i32` because the bit-extraction arithmetic below is signed.
    skip_bits: i32,
    /// Emulation-prevention bytes (`0x03`) skipped during the current read.
    /// Reset to zero at the start of each bit read.
    skipped_bytes: usize,
    /// Total bytes ever passed to [`add`](Self::add) (pre-clamp).
    /// Diagnostic only; reset by [`reset`](Self::reset).
    transfer_length: usize,
}

impl TsStreamBuffer {
    /// Creates an empty buffer. The backing store grows lazily, capped at
    /// [`BUFFER_SIZE`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of stored content bytes.
    #[must_use]
    pub fn length(&self) -> u64 {
        u64::try_from(self.buffer.len()).unwrap_or(u64::MAX)
    }

    /// The current read cursor in bytes.
    #[must_use]
    pub fn position(&self) -> u64 {
        u64::try_from(self.position).unwrap_or(u64::MAX)
    }

    /// Total bytes ever passed to [`add`](Self::add), before the capacity clamp.
    #[must_use]
    pub fn transfer_length(&self) -> u64 {
        u64::try_from(self.transfer_length).unwrap_or(u64::MAX)
    }

    /// Bumps the transfer length by `length` without appending any content (the
    /// M2TS demux uses this for an already-analyzed stream whose bytes it counts
    /// but no longer needs to buffer). The counter wraps on overflow.
    pub const fn add_transfer_length(&mut self, length: usize) {
        self.transfer_length = self.transfer_length.wrapping_add(length);
    }

    /// The stored content bytes — the accumulated PES
    /// payload the codec scanners (and the demux's tests) inspect after
    /// [`add`](Self::add). Distinct from [`transfer_length`](Self::transfer_length),
    /// which also counts bytes that were transferred but not buffered.
    #[must_use]
    pub fn content(&self) -> &[u8] {
        &self.buffer
    }

    /// Bits remaining from the cursor to the end of the stored content
    /// (`(length - position) * 8 - skip_bits`).
    ///
    /// May be negative if the cursor sits past the content; the Exp-Golomb
    /// readers treat any non-positive value as "no bits left".
    #[must_use]
    pub fn data_bit_stream_remain(&self) -> i64 {
        let len = i64::try_from(self.buffer.len()).unwrap_or(i64::MAX);
        let pos = i64::try_from(self.position).unwrap_or(i64::MAX);
        len.wrapping_sub(pos).wrapping_mul(8).wrapping_sub(i64::from(self.skip_bits))
    }

    /// Whole bytes remaining from the cursor to the end of the stored content
    /// (`length - position`). May be
    /// negative if the cursor is past the content.
    #[must_use]
    pub fn data_bit_stream_remain_bytes(&self) -> i64 {
        let len = i64::try_from(self.buffer.len()).unwrap_or(i64::MAX);
        let pos = i64::try_from(self.position).unwrap_or(i64::MAX);
        len.wrapping_sub(pos)
    }

    /// Appends `length` bytes from `buffer[offset..]`.
    ///
    /// The transfer length accumulates the pre-clamp `length`; the stored length
    /// is then clamped so it never exceeds [`BUFFER_SIZE`] (the 5 MiB window).
    /// A `length` of zero, or a source slice too short to satisfy the request, is
    /// a no-op for the stored content (though it still counts toward the
    /// transfer length).
    pub fn add(&mut self, buffer: &[u8], offset: usize, length: usize) {
        self.transfer_length = self.transfer_length.wrapping_add(length);
        let length = if self.buffer.len().saturating_add(length) >= BUFFER_SIZE {
            BUFFER_SIZE.saturating_sub(self.buffer.len())
        } else {
            length
        };
        if length == 0 {
            return;
        }
        if let Some(src) = buffer.get(offset..offset.saturating_add(length)) {
            self.buffer.extend_from_slice(src);
        }
    }

    /// Moves the read cursor.
    ///
    /// A negative resolved position is clamped to zero;
    /// `SeekOrigin::End` is relative to the 5 MiB
    /// logical stream length.
    pub fn seek(&mut self, offset: i64, origin: SeekOrigin) {
        let base = match origin {
            SeekOrigin::Begin => 0_i64,
            SeekOrigin::Current => i64::try_from(self.position).unwrap_or(i64::MAX),
            SeekOrigin::End => i64::try_from(BUFFER_SIZE).unwrap_or(i64::MAX),
        };
        let target = base.wrapping_add(offset);
        self.position = usize::try_from(target).unwrap_or(0);
    }

    /// Discards the stored content and zeroes the transfer length, leaving the
    /// cursor and bit state untouched.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.transfer_length = 0;
    }

    /// Rewinds to the start and clears the bit cursor, ready for a fresh pass
    /// over the content.
    pub const fn begin_read(&mut self) {
        self.skip_bits = 0;
        self.skipped_bytes = 0;
        self.position = 0;
    }

    /// Reads the byte at position `p` over the conceptual zero-filled 5 MiB
    /// stream (`0xFF` — the EOF sentinel — at or past the window's end).
    fn raw_byte_at(&self, p: usize) -> u8 {
        self.buffer.get(p).copied().unwrap_or(if p < BUFFER_SIZE { 0x00 } else { 0xFF })
    }

    /// Reads one byte and advances the cursor.
    ///
    /// With `skip_h26x_emulation_byte`, an H.264/H.265 RBSP emulation-prevention
    /// byte (a `0x03` immediately following `0x00 0x00`) is removed: the byte
    /// *after* the `0x03` is returned instead and `skipped_bytes` is bumped. When
    /// fewer than two bytes precede the cursor the look-back is skipped.
    /// Public so codec scanners can read raw bytes;
    /// pass `false` for `skip_h26x_emulation_byte` to read bytes verbatim.
    #[must_use]
    pub fn read_byte(&mut self, skip_h26x_emulation_byte: bool) -> u8 {
        let p = self.position;
        let mut temp = self.raw_byte_at(p);
        self.position = p.saturating_add(1);
        // The look-back at the two preceding bytes confirms a `00 00` run; the
        // `checked_sub` pair guards the lower bound near the start of the buffer.
        if skip_h26x_emulation_byte
            && temp == 0x03
            && let (Some(p2), Some(p1)) = (p.checked_sub(2), p.checked_sub(1))
            && self.raw_byte_at(p2) == 0x00
            && self.raw_byte_at(p1) == 0x00
        {
            temp = self.raw_byte_at(p.saturating_add(1));
            self.position = p.saturating_add(2);
            self.skipped_bytes = self.skipped_bytes.saturating_add(1);
        }
        temp
    }

    /// Reads `bytes` raw bytes and advances the cursor, or `None` when the read
    /// would reach the end of the stored content.
    ///
    /// The guard is
    /// `position + bytes >= length` (note the inclusive `>=`, so a read that ends
    /// *at* the content end also yields `None`); otherwise the `bytes`-long slice
    /// from the cursor is returned. This does **not**
    /// honour the emulation-prevention skip and leaves the bit cursor (`skip_bits`)
    /// untouched — codecs call it only while byte-aligned. A `bytes` so large the
    /// cursor offset overflows is treated as past-end (`None`).
    #[must_use]
    pub fn read_bytes(&mut self, bytes: usize) -> Option<Vec<u8>> {
        let end = self.position.checked_add(bytes)?;
        if end >= self.buffer.len() {
            return None;
        }
        // `end < len` (and `position <= end`) makes this range valid, so the copy is
        // total — iterate rather than slice-index to keep it panic- and `?`-free.
        let value: Vec<u8> = self.buffer.iter().skip(self.position).take(bytes).copied().collect();
        self.position = end;
        Some(value)
    }

    /// Advances the bit cursor by `bits` and repositions — the shared tail of
    /// every bit read (`skip_bits += bits; position = pos + (skip_bits >> 3) +
    /// skipped_bytes; skip_bits %= 8`).
    fn advance_bits(&mut self, pos: usize, bits: usize) {
        let bits = i32::try_from(bits).unwrap_or(i32::MAX);
        self.skip_bits = self.skip_bits.wrapping_add(bits);
        let whole = usize::try_from(self.skip_bits.wrapping_shr(3)).unwrap_or(0);
        self.position = pos.saturating_add(whole).saturating_add(self.skipped_bytes);
        self.skip_bits = self.skip_bits.wrapping_rem(8);
    }

    /// Reads a single bit as a `bool`.
    ///
    /// Returns `false` without consuming anything when the cursor is exactly at
    /// the end of the stored content.
    #[must_use]
    pub fn read_bool(&mut self, skip_h26x_emulation_byte: bool) -> bool {
        let pos = self.position;
        self.skipped_bytes = 0;
        if pos == self.buffer.len() {
            return false;
        }
        let data = self.read_byte(skip_h26x_emulation_byte);
        let sc = 8_i32.wrapping_sub(self.skip_bits).wrapping_sub(1);
        let mask = 1_u32.wrapping_shl(sc.cast_unsigned());
        let value = (u32::from(data) & mask) != 0;
        self.advance_bits(pos, 1);
        value
    }

    /// Loads up to four bytes from the cursor, big-endian, into the low-to-high
    /// bytes of a `u32` (`b0 << 24 | b1 << 16 | b2 << 8 | b3`) — the four-byte
    /// shift-accumulate window shared by [`read_bits4`](Self::read_bits4)
    /// and [`read_bits8`](Self::read_bits8).
    ///
    /// The in-bounds test deliberately uses the *original* `pos` (`pos + i`) —
    /// so the loop count is governed by `pos` while the reads
    /// continue from the advancing cursor; [`read_bits8`](Self::read_bits8) relies
    /// on that quirk by passing the same `pos` to both of its loads.
    fn load_be_word(&mut self, pos: usize, skip_h26x_emulation_byte: bool) -> u32 {
        let mut shift: u32 = 24;
        let mut data: u32 = 0;
        for i in 0..4_usize {
            if pos.saturating_add(i) >= self.buffer.len() {
                break;
            }
            let byte = u32::from(self.read_byte(skip_h26x_emulation_byte));
            data = data.wrapping_add(byte.wrapping_shl(shift));
            shift = shift.wrapping_sub(8);
        }
        data
    }

    /// Reads up to 16 bits MSB-first into a `u16`.
    ///
    /// Bits come from a two-byte big-endian window loaded at the read's **byte**
    /// position, so the readable span is fixed at 16 bits regardless of the
    /// current bit offset. A read whose `skip_bits + bits` exceeds 16 returns the
    /// `16 - skip_bits` real bits then **zero**-filled tail bits — it never reads
    /// further into the stream. Every in-tree call site is byte-aligned, so a
    /// full-width `read_bits2(16, …)` always starts at offset 0 and this never
    /// bites; a new codec scanner requesting full width at a non-zero bit offset
    /// would silently corrupt the tail. Pinned by
    /// `read_bits2_past_window_zero_extends`.
    #[must_use]
    pub fn read_bits2(&mut self, bits: usize, skip_h26x_emulation_byte: bool) -> u16 {
        let pos = self.position;
        self.skipped_bytes = 0;
        let mut shift: u32 = 8;
        let mut data: u32 = 0;
        for i in 0..2_usize {
            if pos.saturating_add(i) >= self.buffer.len() {
                break;
            }
            let byte = u32::from(self.read_byte(skip_h26x_emulation_byte));
            data = data.wrapping_add(byte.wrapping_shl(shift));
            shift = shift.wrapping_sub(8);
        }
        let count = i32::try_from(bits).unwrap_or(i32::MAX);
        let end = self.skip_bits.wrapping_add(count);
        let mut value: u16 = 0;
        let mut i = self.skip_bits;
        while i < end {
            let sc = 16_i32.wrapping_sub(i).wrapping_sub(1);
            let mask = 1_u32.wrapping_shl(sc.cast_unsigned());
            value = value.wrapping_shl(1).wrapping_add(u16::from((data & mask) != 0));
            i = i.wrapping_add(1);
        }
        self.advance_bits(pos, bits);
        value
    }

    /// Reads up to 32 bits MSB-first into a `u32`.
    ///
    /// Bits come from a four-byte big-endian window loaded at the read's **byte**
    /// position; the readable span is fixed at 32 bits. A read whose
    /// `skip_bits + bits` exceeds 32 does **not** read past the window into the
    /// stream — and, unlike [`read_bits2`](Self::read_bits2) /
    /// [`read_bits8`](Self::read_bits8) (whose tails zero-fill), the past-window
    /// mask here wraps back into the window's high bits, so the tail repeats the
    /// window top (effectively `window << skip_bits`). Latent: every in-tree call
    /// site is byte-aligned (`read_bits4(32, …)` at offset 0); a new unaligned
    /// full-width caller would get corrupt tail bits, not the true stream bits.
    /// Pinned by `read_bits4_unaligned_full_width_wraps_not_zero_fills`.
    #[must_use]
    pub fn read_bits4(&mut self, bits: usize, skip_h26x_emulation_byte: bool) -> u32 {
        let pos = self.position;
        self.skipped_bytes = 0;
        let data = self.load_be_word(pos, skip_h26x_emulation_byte);
        let count = i32::try_from(bits).unwrap_or(i32::MAX);
        let end = self.skip_bits.wrapping_add(count);
        let mut value: u32 = 0;
        let mut i = self.skip_bits;
        while i < end {
            let sc = 32_i32.wrapping_sub(i).wrapping_sub(1);
            let mask = 1_u32.wrapping_shl(sc.cast_unsigned());
            value = value.wrapping_shl(1).wrapping_add(u32::from((data & mask) != 0));
            i = i.wrapping_add(1);
        }
        self.advance_bits(pos, bits);
        value
    }

    /// Reads up to 64 bits MSB-first into a `u64`.
    ///
    /// The 64-bit window is two four-byte big-endian loads (the second reusing
    /// the same `pos`-based in-bounds test as the first), loaded at the read's
    /// **byte** position; the readable span is fixed at 64 bits. A read whose
    /// `skip_bits + bits` exceeds 64 returns the real bits then **zero**-filled
    /// tail bits (the explicit `(0..64)` index guard below), never reading past
    /// the window into the stream. Latent — every in-tree call site is
    /// byte-aligned. Pinned by `read_bits8_past_window_contributes_zero`.
    #[must_use]
    pub fn read_bits8(&mut self, bits: usize, skip_h26x_emulation_byte: bool) -> u64 {
        let pos = self.position;
        self.skipped_bytes = 0;
        let data = self.load_be_word(pos, skip_h26x_emulation_byte);
        let data2 = self.load_be_word(pos, skip_h26x_emulation_byte);
        let window = u64::from(data).wrapping_shl(32).wrapping_add(u64::from(data2));
        let count = i32::try_from(bits).unwrap_or(i32::MAX);
        let end = self.skip_bits.wrapping_add(count);
        let mut value: u64 = 0;
        let mut i = self.skip_bits;
        while i < end {
            let idx = 64_i32.wrapping_sub(i).wrapping_sub(1);
            let bit = if (0..64).contains(&idx) {
                window.wrapping_shr(idx.cast_unsigned()) & 1
            } else {
                0
            };
            value = value.wrapping_shl(1).wrapping_add(bit);
            i = i.wrapping_add(1);
        }
        self.advance_bits(pos, bits);
        value
    }

    /// Skips `bits` bits, reading them in 16-bit chunks
    /// (`ceil(bits / 16)` calls to [`read_bits2`](Self::read_bits2)).
    pub fn bs_skip_bits(&mut self, bits: usize, skip_h26x_emulation_byte: bool) {
        let count = bits.div_ceil(16);
        let mut bits_read: usize = 0;
        for _ in 0..count {
            let to_read = bits.wrapping_sub(bits_read).min(16);
            let _ = self.read_bits2(to_read, skip_h26x_emulation_byte);
            bits_read = bits_read.wrapping_add(to_read);
        }
    }

    /// Skips to the next whole-byte boundary
    /// (a no-op when already byte-aligned).
    pub fn bs_skip_next_byte(&mut self) {
        if self.skip_bits > 0 {
            let bits = usize::try_from(8_i32.wrapping_sub(self.skip_bits)).unwrap_or(0);
            self.bs_skip_bits(bits, false);
        }
    }

    /// Clears the bit cursor without moving the byte cursor.
    pub const fn bs_reset_bits(&mut self) {
        self.skip_bits = 0;
    }

    /// Skips `bytes` whole bytes.
    ///
    /// A positive count reads (and discards) that many bytes, honoring the
    /// emulation-prevention skip. A non-positive count repositions the cursor to
    /// `pos + (skip_bits >> 3) + bytes` (so it can move backwards), clamping a
    /// negative result to zero.
    pub fn bs_skip_bytes(&mut self, bytes: i32, skip_h26x_emulation_byte: bool) {
        if bytes > 0 {
            for _ in 0..bytes {
                let _ = self.read_byte(skip_h26x_emulation_byte);
            }
        } else {
            let pos = self.position;
            let delta = self.skip_bits.wrapping_shr(3).wrapping_add(bytes);
            let target = i64::try_from(pos).unwrap_or(i64::MAX).wrapping_add(i64::from(delta));
            self.position = usize::try_from(target).unwrap_or(0);
        }
    }

    /// Counts the leading zero bits of an Exp-Golomb code, consuming the
    /// terminating one-bit — the `while` loop shared by [`read_exp`](Self::read_exp)
    /// and [`skip_exp`](Self::skip_exp). The remaining-bits guard stops the
    /// scan at end-of-content; the `u8` counter deliberately wraps.
    fn count_exp_leading_zeroes(&mut self, skip_h26x_emulation_byte: bool) -> u8 {
        let mut leading_zeroes: u8 = 0;
        while self.data_bit_stream_remain() > 0 && !self.read_bool(skip_h26x_emulation_byte) {
            leading_zeroes = leading_zeroes.wrapping_add(1);
        }
        leading_zeroes
    }

    /// Reads an unsigned Exp-Golomb code `ue(v)`.
    ///
    /// Counts `n` leading zeros, then reads `n` more bits: the value is
    /// `2^n - 1 + suffix`, computed with integer `1 << n` (exact for every
    /// valid code).
    #[must_use]
    pub fn read_exp(&mut self, skip_h26x_emulation_byte: bool) -> u32 {
        let leading_zeroes = self.count_exp_leading_zeroes(skip_h26x_emulation_byte);
        let info = 1_u32.wrapping_shl(u32::from(leading_zeroes));
        let extra = self.read_bits4(usize::from(leading_zeroes), skip_h26x_emulation_byte);
        info.wrapping_sub(1).wrapping_add(extra)
    }

    /// Skips one unsigned Exp-Golomb code (count the
    /// leading zeros, then skip that many suffix bits).
    pub fn skip_exp(&mut self, skip_h26x_emulation_byte: bool) {
        let leading_zeroes = self.count_exp_leading_zeroes(skip_h26x_emulation_byte);
        self.bs_skip_bits(usize::from(leading_zeroes), skip_h26x_emulation_byte);
    }

    /// Skips `num` consecutive Exp-Golomb codes.
    pub fn skip_exp_multi(&mut self, num: usize, skip_h26x_emulation_byte: bool) {
        for _ in 0..num {
            self.skip_exp(skip_h26x_emulation_byte);
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest};

    use super::{BUFFER_SIZE, SeekOrigin, TsStreamBuffer};

    /// Builds a buffer holding `data`, rewound for reading.
    fn buf(data: &[u8]) -> TsStreamBuffer {
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        b
    }

    /// Packs `bits` MSB-first into bytes; a trailing partial byte is left-aligned
    /// (low bits padded with zero), matching how the readers consume them.
    fn pack_bits(bits: &[bool]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut cur: u8 = 0;
        let mut count: u32 = 0;
        for &bit in bits {
            cur = cur.wrapping_shl(1).wrapping_add(u8::from(bit));
            count = count.wrapping_add(1);
            if count == 8 {
                bytes.push(cur);
                cur = 0;
                count = 0;
            }
        }
        if count > 0 {
            bytes.push(cur.wrapping_shl(8_u32.wrapping_sub(count)));
        }
        bytes
    }

    /// Encodes `v` as an unsigned Exp-Golomb code `ue(v)` into MSB-first bytes:
    /// `n` leading zeros, then the `(n + 1)`-bit binary of `v + 1`.
    fn encode_ue(v: u32) -> Vec<u8> {
        let code_num = v.wrapping_add(1);
        let n = 31_u32.wrapping_sub(code_num.leading_zeros());
        // `n` leading zeros, then the (n + 1)-bit binary of `code_num` MSB-first.
        let mut bits: Vec<bool> = vec![false; usize::try_from(n).unwrap()];
        for i in (0..=n).rev() {
            bits.push((code_num.wrapping_shr(i) & 1) == 1);
        }
        pack_bits(&bits)
    }

    #[test]
    fn new_is_empty() {
        let b = TsStreamBuffer::new();
        assert_eq!(b.length(), 0);
        assert_eq!(b.position(), 0);
        assert_eq!(b.transfer_length(), 0);
        assert_eq!(b.data_bit_stream_remain(), 0);
        assert_eq!(b.data_bit_stream_remain_bytes(), 0);
    }

    #[test]
    fn default_is_empty() {
        let b = TsStreamBuffer::default();
        assert_eq!(b.length(), 0);
        assert_eq!(b.position(), 0);
    }

    #[test]
    fn add_appends_and_tracks_transfer_length() {
        let mut b = TsStreamBuffer::new();
        b.add(&[0x01, 0x02, 0x03], 0, 3);
        assert_eq!(b.length(), 3);
        assert_eq!(b.transfer_length(), 3);
        b.add(&[0x04, 0x05], 0, 2);
        assert_eq!(b.length(), 5);
        assert_eq!(b.transfer_length(), 5);
    }

    #[test]
    fn add_transfer_length_bumps_count_without_content() {
        let mut b = TsStreamBuffer::new();
        b.add(&[0x01, 0x02], 0, 2);
        assert_eq!(b.length(), 2);
        assert_eq!(b.transfer_length(), 2);
        // Bump the transfer length without appending: content length is unchanged.
        b.add_transfer_length(100);
        assert_eq!(b.length(), 2);
        assert_eq!(b.transfer_length(), 102);
        b.add_transfer_length(0);
        assert_eq!(b.transfer_length(), 102);
    }

    #[test]
    fn content_exposes_stored_bytes() {
        let mut b = TsStreamBuffer::new();
        assert!(b.content().is_empty());
        b.add(&[0xDE, 0xAD, 0xBE, 0xEF], 1, 2);
        assert_eq!(b.content(), [0xAD, 0xBE]);
        // A pure transfer-length bump does not change the content view.
        b.add_transfer_length(50);
        assert_eq!(b.content(), [0xAD, 0xBE]);
        b.reset();
        assert!(b.content().is_empty());
    }

    #[test]
    fn add_honors_offset() {
        let mut b = TsStreamBuffer::new();
        b.add(&[0x00, 0x01, 0x02, 0x03, 0x04], 2, 2);
        assert_eq!(b.length(), 2);
        b.begin_read();
        // The stored bytes are [0x02, 0x03]: their top bits are 0,0.
        assert!(!b.read_bool(false));
        assert!(!b.read_bool(false));
    }

    #[test]
    fn add_zero_length_is_noop() {
        let mut b = TsStreamBuffer::new();
        b.add(&[], 0, 0);
        assert_eq!(b.length(), 0);
        assert_eq!(b.transfer_length(), 0);
        // A non-empty source with length 0 still stores nothing.
        b.add(&[0xAB, 0xCD], 0, 0);
        assert_eq!(b.length(), 0);
        assert_eq!(b.transfer_length(), 0);
    }

    #[test]
    fn add_short_source_stores_nothing_but_counts_transfer() {
        let mut b = TsStreamBuffer::new();
        // Asks for 5 bytes from a 2-byte source: stored content unchanged, but
        // the transfer length still accumulates the requested length.
        b.add(&[0x01, 0x02], 0, 5);
        assert_eq!(b.length(), 0);
        assert_eq!(b.transfer_length(), 5);
    }

    #[test]
    fn add_clamps_to_buffer_size() {
        let mut b = TsStreamBuffer::new();
        let big = vec![0x5A_u8; BUFFER_SIZE.saturating_add(16)];
        b.add(&big, 0, big.len());
        // Stored length is clamped to the 5 MiB cap; the transfer length is not.
        assert_eq!(b.length(), u64::try_from(BUFFER_SIZE).unwrap());
        assert_eq!(b.transfer_length(), u64::try_from(BUFFER_SIZE.saturating_add(16)).unwrap());
        // A further add against the full buffer clamps the stored length to zero
        // (no growth) yet still counts toward the transfer length.
        b.add(&[0x99], 0, 1);
        assert_eq!(b.length(), u64::try_from(BUFFER_SIZE).unwrap());
        assert_eq!(b.transfer_length(), u64::try_from(BUFFER_SIZE.saturating_add(17)).unwrap());
    }

    #[test]
    fn reset_clears_content_and_transfer_length() {
        let mut b = TsStreamBuffer::new();
        b.add(&[0x01, 0x02, 0x03], 0, 3);
        b.reset();
        assert_eq!(b.length(), 0);
        assert_eq!(b.transfer_length(), 0);
        // The buffer is reusable after a reset.
        b.add(&[0x04], 0, 1);
        assert_eq!(b.length(), 1);
    }

    #[test]
    fn seek_begin_current_end_and_clamp() {
        let mut b = buf(&[0x00, 0x11, 0x22, 0x33]);
        b.seek(3, SeekOrigin::Begin);
        assert_eq!(b.position(), 3);
        b.seek(2, SeekOrigin::Current);
        assert_eq!(b.position(), 5);
        b.seek(-1, SeekOrigin::End);
        assert_eq!(b.position(), u64::try_from(BUFFER_SIZE).unwrap().saturating_sub(1));
        // A negative resolved position clamps to zero.
        b.seek(-100, SeekOrigin::Begin);
        assert_eq!(b.position(), 0);
    }

    #[test]
    fn begin_read_rewinds() {
        let mut b = buf(&[0xFF, 0xFF]);
        let _ = b.read_bool(false);
        b.seek(2, SeekOrigin::Begin);
        b.begin_read();
        assert_eq!(b.position(), 0);
    }

    #[test]
    fn remain_counts_bits_and_bytes() {
        let b = buf(&[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(b.data_bit_stream_remain(), 32);
        assert_eq!(b.data_bit_stream_remain_bytes(), 4);
    }

    #[test]
    fn remain_is_negative_past_content() {
        let mut b = buf(&[0xAA]);
        b.seek(5, SeekOrigin::Begin);
        assert_eq!(b.data_bit_stream_remain(), -32);
        assert_eq!(b.data_bit_stream_remain_bytes(), -4);
    }

    #[test]
    fn read_byte_reads_sequentially() {
        let mut b = buf(&[0x12, 0x34, 0x56]);
        assert_eq!(b.read_byte(false), 0x12);
        assert_eq!(b.read_byte(false), 0x34);
        assert_eq!(b.read_byte(false), 0x56);
    }

    #[test]
    fn read_bytes_reads_a_slice_and_advances() {
        let mut b = buf(&[0x0B, 0x77, 0x24, 0x40, 0xE1]);
        // Position 0 + 2 < length 5 → reads the first two bytes, cursor at 2.
        assert_eq!(b.read_bytes(2).as_deref(), Some(&[0x0B, 0x77][..]));
        assert_eq!(b.position(), 2);
        // Position 2 + 2 < length 5 → reads two more, cursor at 4.
        assert_eq!(b.read_bytes(2).as_deref(), Some(&[0x24, 0x40][..]));
        assert_eq!(b.position(), 4);
    }

    #[test]
    fn read_bytes_at_or_past_end_is_none() {
        let mut b = buf(&[0x0B, 0x77, 0x24, 0x40]);
        // A read that would END exactly at the content length yields None (the
        // inclusive `position + bytes >= length` guard), leaving the cursor put.
        assert_eq!(b.read_bytes(4), None);
        assert_eq!(b.position(), 0);
        // One byte short of the end is allowed.
        assert_eq!(b.read_bytes(3).as_deref(), Some(&[0x0B, 0x77, 0x24][..]));
        assert_eq!(b.position(), 3);
        // A read past the end is None and does not move the cursor.
        assert_eq!(b.read_bytes(5), None);
        assert_eq!(b.position(), 3);
    }

    #[test]
    fn read_bytes_zero_length_when_in_content_is_empty() {
        let mut b = buf(&[0x0B, 0x77]);
        // Position 0 + 0 < length 2 → an empty slice (not None).
        assert_eq!(b.read_bytes(0).as_deref(), Some(&[][..]));
        assert_eq!(b.position(), 0);
        // At the content end, even a zero-length read is None.
        b.seek(2, SeekOrigin::Begin);
        assert_eq!(b.read_bytes(0), None);
    }

    #[test]
    fn read_bytes_overflow_is_none() {
        let mut b = buf(&[0x0B, 0x77, 0x24]);
        b.seek(1, SeekOrigin::Begin);
        // A length that overflows the cursor offset is treated as past-end.
        assert_eq!(b.read_bytes(usize::MAX), None);
        assert_eq!(b.position(), 1);
    }

    #[test]
    fn read_byte_no_skip_keeps_emulation_byte() {
        // 00 00 03 ..; without the skip flag the 0x03 is returned verbatim.
        let mut b = buf(&[0x00, 0x00, 0x03, 0x5A]);
        b.seek(2, SeekOrigin::Begin);
        assert_eq!(b.read_byte(false), 0x03);
    }

    #[test]
    fn read_byte_skips_emulation_byte_after_double_zero() {
        // 00 00 03 5A: reading at the 0x03 with skip returns the byte after it.
        let mut b = buf(&[0x00, 0x00, 0x03, 0x5A]);
        b.seek(2, SeekOrigin::Begin);
        assert_eq!(b.read_byte(true), 0x5A);
        // Cursor advanced past both the 0x03 and the 0x5A (position 2 -> 4).
        assert_eq!(b.position(), 4);
    }

    #[test]
    fn read_byte_no_skip_when_prefix_not_double_zero() {
        // First preceding byte non-zero: no emulation removal.
        let mut b = buf(&[0x01, 0x00, 0x03, 0x5A]);
        b.seek(2, SeekOrigin::Begin);
        assert_eq!(b.read_byte(true), 0x03);
        // Second preceding byte non-zero: no removal either.
        let mut b = buf(&[0x00, 0x01, 0x03, 0x5A]);
        b.seek(2, SeekOrigin::Begin);
        assert_eq!(b.read_byte(true), 0x03);
    }

    #[test]
    fn read_byte_no_skip_when_too_close_to_start() {
        // A 0x03 at position 0/1 cannot look back two bytes -> returned as-is.
        let mut b = buf(&[0x03, 0x5A]);
        assert_eq!(b.read_byte(true), 0x03);
        let mut b = buf(&[0x00, 0x03, 0x5A]);
        b.seek(1, SeekOrigin::Begin);
        assert_eq!(b.read_byte(true), 0x03);
    }

    #[test]
    fn read_bool_reads_bits_msb_first() {
        // 0xAC = 1010_1100.
        let mut b = buf(&[0xAC]);
        let bits: Vec<bool> = (0..8).map(|_| b.read_bool(false)).collect();
        assert_eq!(bits, vec![true, false, true, false, true, true, false, false]);
        // After eight bits the cursor has advanced exactly one byte.
        assert_eq!(b.position(), 1);
    }

    #[test]
    fn read_bool_at_end_is_false() {
        let mut b = buf(&[0xFF]);
        for _ in 0..8 {
            assert!(b.read_bool(false));
        }
        // Cursor now at the end: further reads return false without consuming.
        assert!(!b.read_bool(false));
        assert_eq!(b.position(), 1);
    }

    #[test]
    fn read_bool_past_content_reads_zero_fill() {
        // Seek beyond the stored content but within the 5 MiB cap: the byte source
        // is the zero-filled region, so the bit is 0 (false).
        let mut b = buf(&[0xFF]);
        b.seek(10, SeekOrigin::Begin);
        assert!(!b.read_bool(false));
    }

    #[test]
    fn read_bool_at_eof_sentinel_reads_one_fill() {
        // At/after the 5 MiB cap the byte source is 0xFF, so the top bit is 1.
        let mut b = buf(&[0xAA]);
        b.seek(i64::try_from(BUFFER_SIZE).unwrap(), SeekOrigin::Begin);
        assert!(b.read_bool(false));
    }

    #[test]
    fn read_bool_through_emulation_byte() {
        // 00 00 03 80: after consuming 00 00, a single bit read with skip lands on
        // 0x80 (top bit 1), the 0x03 having been removed.
        let mut b = buf(&[0x00, 0x00, 0x03, 0x80]);
        b.seek(2, SeekOrigin::Begin);
        assert!(b.read_bool(true));
    }

    #[test]
    fn read_bits4_reads_msb_first() {
        // 0xAC = 1010_1100: read 3 then 5 bits, MSB-first.
        let mut b = buf(&[0xAC]);
        assert_eq!(b.read_bits4(3, false), 0b101); // 5
        assert_eq!(b.read_bits4(5, false), 12); // 0_1100
        assert_eq!(b.position(), 1);
    }

    #[test]
    fn read_bits4_single_byte_and_full_widths() {
        let mut b = buf(&[0xAC]);
        assert_eq!(b.read_bits4(8, false), 0xAC);
        let mut b = buf(&[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(b.read_bits4(32, false), 0x1234_5678);
    }

    #[test]
    fn read_bits4_truncated_window_zero_fills() {
        // Only two of the four window bytes are present; the rest read as zero.
        let mut b = buf(&[0x12, 0x34]);
        assert_eq!(b.read_bits4(32, false), 0x1234_0000);
    }

    #[test]
    fn read_bits2_reads_msb_first() {
        // 0xAC = 1010_1100: two nibbles, then a whole byte.
        let mut b = buf(&[0xAC]);
        assert_eq!(b.read_bits2(4, false), 0xA);
        assert_eq!(b.read_bits2(4, false), 0xC);
        let mut b = buf(&[0xAC]);
        assert_eq!(b.read_bits2(8, false), 0xAC);
        let mut b = buf(&[0x12, 0x34]);
        assert_eq!(b.read_bits2(16, false), 0x1234);
    }

    #[test]
    fn read_bits2_past_window_zero_extends() {
        // After 3 bits, reading 16 more runs 3 bits past the 16-bit window; those
        // contribute zero (the window is zero-extended).
        let mut b = buf(&[0xFF, 0xFF]);
        assert_eq!(b.read_bits2(3, false), 0b111);
        assert_eq!(b.read_bits2(16, false), 0xFFF8);
    }

    #[test]
    fn read_bits8_full_and_partial_widths() {
        let bytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];
        assert_eq!(buf(&bytes).read_bits8(64, false), 0x0123_4567_89AB_CDEF);
        assert_eq!(buf(&bytes).read_bits8(8, false), 0x01);
        assert_eq!(buf(&bytes).read_bits8(16, false), 0x0123);
        // Top 33 bits = the 64-bit value >> 31.
        assert_eq!(buf(&bytes).read_bits8(33, false), 0x0246_8ACF);
    }

    #[test]
    fn read_bits8_truncated_window_zero_fills() {
        let mut b = buf(&[0x12, 0x34]);
        assert_eq!(b.read_bits8(16, false), 0x1234);
    }

    #[test]
    fn read_bits8_past_window_contributes_zero() {
        // Skip one bit, then read 64: the 64th position is past the window and
        // contributes 0.
        let bytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];
        let mut b = buf(&bytes);
        let _ = b.read_bool(false);
        assert_eq!(b.read_bits8(64, false), 0x0246_8ACF_1357_9BDE);
    }

    #[test]
    fn read_bits4_unaligned_full_width_wraps_not_zero_fills() {
        // read_bits2/4/8 load a fixed 2/4/8-byte window at the read's byte
        // position, so a full-width read at a non-zero bit offset runs
        // past the window and never reads the true downstream bits. Unlike
        // read_bits2 / read_bits8 — whose tails zero-fill — read_bits4's
        // negative-shift mask wraps back into the 32-bit window's high bits, so the
        // tail repeats the window top (effectively `window << skip_bits`). Every
        // in-tree call site is byte-aligned, so this never bites; this pins the
        // behavior so an unaligned full-width caller is a noticed change.
        let mut b = buf(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(b.read_bits4(3, false), 0b111);
        assert_eq!(b.read_bits4(32, false), 0xFFFF_FFFF); // wraps to all-ones, not 0xFFFF_FFF8
        let mut b = buf(&[0x12, 0x34, 0x56, 0x78, 0x9A]);
        assert_eq!(b.read_bits4(3, false), 0x0);
        assert_eq!(b.read_bits4(32, false), 0x91A2_B3C0); // window (0x12345678) << 3, wrapped
    }

    #[test]
    fn bs_skip_bits_zero_is_noop() {
        let mut b = buf(&[0xAC]);
        b.bs_skip_bits(0, false);
        assert_eq!(b.position(), 0);
        assert!(b.read_bool(false)); // still at bit 0 of 0xAC (= 1)
    }

    #[test]
    fn bs_skip_bits_within_and_across_bytes() {
        // 0xAC = 1010_1100: skip 5 bits, the next 3 are 100.
        let mut b = buf(&[0xAC, 0xFF]);
        b.bs_skip_bits(5, false);
        assert_eq!(b.read_bits4(3, false), 0b100);
        // 20 bits = two read_bits2 chunks (16 + 4) -> byte 2, bit 4.
        let mut b = buf(&[0x00, 0x00, 0x00, 0xFF]);
        b.bs_skip_bits(20, false);
        assert_eq!(b.position(), 2);
        assert_eq!(b.read_bits4(4, false), 0); // remaining nibble of byte 2
    }

    #[test]
    fn bs_skip_next_byte_aligns_then_is_noop() {
        let mut b = buf(&[0xAC, 0x3C]);
        let _ = b.read_bits4(3, false); // bit cursor now at 3
        b.bs_skip_next_byte();
        assert_eq!(b.position(), 1);
        b.bs_skip_next_byte(); // already aligned -> no-op
        assert_eq!(b.position(), 1);
        assert_eq!(b.read_bits4(8, false), 0x3C);
    }

    #[test]
    fn bs_reset_bits_clears_cursor_without_moving() {
        let mut b = buf(&[0xAC]);
        let _ = b.read_bits4(3, false);
        b.bs_reset_bits();
        assert_eq!(b.position(), 0);
        // Reading restarts at bit 0 of byte 0: top 3 bits of 0xAC = 101.
        assert_eq!(b.read_bits4(3, false), 0b101);
    }

    #[test]
    fn bs_skip_bytes_positive_reads_forward() {
        let mut b = buf(&[0x11, 0x22, 0x33, 0x44]);
        b.bs_skip_bytes(3, false);
        assert_eq!(b.position(), 3);
        assert_eq!(b.read_bits4(8, false), 0x44);
    }

    #[test]
    fn bs_skip_bytes_positive_honors_emulation() {
        // 00 00 03 5A 99: skipping one byte from position 2 with the flag removes
        // the 0x03 and consumes 0x5A, landing at position 4.
        let mut b = buf(&[0x00, 0x00, 0x03, 0x5A, 0x99]);
        b.seek(2, SeekOrigin::Begin);
        b.bs_skip_bytes(1, true);
        assert_eq!(b.position(), 4);
    }

    #[test]
    fn bs_skip_bytes_nonpositive_repositions_and_clamps() {
        let mut b = buf(&[0x11, 0x22, 0x33, 0x44, 0x55]);
        b.seek(4, SeekOrigin::Begin);
        b.bs_skip_bytes(-2, false);
        assert_eq!(b.position(), 2);
        b.bs_skip_bytes(0, false); // no-op for an in-byte cursor
        assert_eq!(b.position(), 2);
        b.bs_skip_bytes(-100, false); // negative result clamps to zero
        assert_eq!(b.position(), 0);
    }

    #[test]
    fn read_exp_decodes_small_codes() {
        // ue(v) for 0,1,2,3 packed MSB-first: "1 010 011 00100" -> 0xA6 0x40.
        let mut b = buf(&[0xA6, 0x40]);
        assert_eq!(b.read_exp(false), 0);
        assert_eq!(b.read_exp(false), 1);
        assert_eq!(b.read_exp(false), 2);
        assert_eq!(b.read_exp(false), 3);
    }

    #[test]
    fn read_exp_decodes_code_with_suffix() {
        // "00111" -> 2^2 - 1 + 0b11 = 6.
        let mut b = buf(&[0x38]);
        assert_eq!(b.read_exp(false), 6);
    }

    #[test]
    fn read_exp_all_zeros_exhausts_via_remain_guard() {
        // No terminating 1-bit in one byte: eight leading zeros, then a suffix read
        // past the content (zero) -> 2^8 - 1 = 255.
        let mut b = buf(&[0x00]);
        assert_eq!(b.read_exp(false), 255);
    }

    #[test]
    fn read_exp_skips_emulation_byte() {
        // 00 00 03 80: read from the 0x03 with the skip flag — the emulation byte
        // is removed and 0x80's leading 1 terminates immediately -> value 0.
        let mut b = buf(&[0x00, 0x00, 0x03, 0x80]);
        b.seek(2, SeekOrigin::Begin);
        assert_eq!(b.read_exp(true), 0);
        // Without the flag the 0x03 is consumed as data, so the value differs.
        let mut b = buf(&[0x00, 0x00, 0x03, 0x80]);
        b.seek(2, SeekOrigin::Begin);
        assert_ne!(b.read_exp(false), 0);
    }

    #[test]
    fn read_bits4_skips_emulation_byte() {
        // 00 00 03 AB CD: an 8-bit read from the 0x03 with the flag yields 0xAB,
        // landing the cursor past both the removed 0x03 and the 0xAB.
        let mut b = buf(&[0x00, 0x00, 0x03, 0xAB, 0xCD]);
        b.seek(2, SeekOrigin::Begin);
        assert_eq!(b.read_bits4(8, true), 0xAB);
        assert_eq!(b.position(), 4);
    }

    #[test]
    fn skip_exp_consumes_one_code() {
        let mut b = buf(&[0xA6, 0x40]);
        b.skip_exp(false); // "1" (value 0)
        b.skip_exp(false); // "010" (value 1)
        assert_eq!(b.read_exp(false), 2);
    }

    #[test]
    fn skip_exp_multi_consumes_n_codes() {
        let mut b = buf(&[0xA6, 0x40]);
        b.skip_exp_multi(2, false);
        assert_eq!(b.read_exp(false), 2);
        // Zero codes is a no-op.
        let mut b = buf(&[0xA6, 0x40]);
        b.skip_exp_multi(0, false);
        assert_eq!(b.read_exp(false), 0);
    }

    #[test]
    fn pack_bits_whole_and_partial_bytes() {
        // Exactly 8 bits -> one whole byte, no trailing partial.
        assert_eq!(pack_bits(&[true, false, true, false, true, false, true, false]), vec![0xAA]);
        // 12 bits -> a whole byte plus a left-aligned partial byte.
        let mut bits = vec![true; 8];
        bits.extend_from_slice(&[true, false, true, false]);
        assert_eq!(pack_bits(&bits), vec![0xFF, 0xA0]);
    }

    proptest! {
        #[test]
        fn read_bool_reconstructs_bytes_msb_first(data in any::<Vec<u8>>()) {
            // Eight MSB-first bits per byte must rebuild the original byte exactly.
            let mut b = buf(&data);
            for &expected in &data {
                let mut got: u8 = 0;
                for _ in 0..8 {
                    got = got.wrapping_shl(1).wrapping_add(u8::from(b.read_bool(false)));
                }
                prop_assert_eq!(got, expected);
            }
            // Consuming every bit lands the cursor exactly at the content end.
            prop_assert_eq!(b.position(), u64::try_from(data.len()).unwrap());
        }

        #[test]
        fn reads_never_panic_on_arbitrary_input(data in any::<Vec<u8>>(), n in 0_usize..512) {
            // Arbitrary bytes, arbitrary read count, emulation path engaged: no
            // panic, and the cursor never runs past the backing cap.
            let mut b = buf(&data);
            for _ in 0..n {
                let _ = b.read_bool(true);
            }
            prop_assert!(b.position() <= u64::try_from(BUFFER_SIZE).unwrap());
        }

        #[test]
        fn read_bytes_matches_slice_or_is_none(data in any::<Vec<u8>>(), n in 0_usize..600) {
            // read_bytes(n) returns the n-byte slice from the cursor exactly when
            // n ends strictly before the content end, else None — and never panics.
            let mut b = buf(&data);
            let got = b.read_bytes(n);
            if n < data.len() {
                prop_assert_eq!(got.as_deref(), data.get(0..n));
                prop_assert_eq!(b.position(), u64::try_from(n).unwrap());
            } else {
                prop_assert_eq!(got, None);
                prop_assert_eq!(b.position(), 0);
            }
        }

        #[test]
        fn read_bits2_byte_at_a_time_reconstructs(data in any::<Vec<u8>>()) {
            let mut b = buf(&data);
            for &expected in &data {
                prop_assert_eq!(b.read_bits2(8, false), u16::from(expected));
            }
        }

        #[test]
        fn read_bits4_byte_at_a_time_reconstructs(data in any::<Vec<u8>>()) {
            let mut b = buf(&data);
            for &expected in &data {
                prop_assert_eq!(b.read_bits4(8, false), u32::from(expected));
            }
        }

        #[test]
        fn read_bits8_byte_at_a_time_reconstructs(data in any::<Vec<u8>>()) {
            let mut b = buf(&data);
            for &expected in &data {
                prop_assert_eq!(b.read_bits8(8, false), u64::from(expected));
            }
        }

        #[test]
        fn read_bits2_matches_bit_by_bit(bytes in any::<[u8; 8]>(), n in 1_usize..=16) {
            // Reading n bits in one call equals accumulating n single-bit reads.
            let mut whole = buf(&bytes);
            let mut bitwise = buf(&bytes);
            let mut acc: u16 = 0;
            for _ in 0..n {
                acc = acc.wrapping_shl(1).wrapping_add(u16::from(bitwise.read_bool(false)));
            }
            prop_assert_eq!(whole.read_bits2(n, false), acc);
        }

        #[test]
        fn read_bits4_matches_bit_by_bit(bytes in any::<[u8; 8]>(), n in 1_usize..=32) {
            let mut whole = buf(&bytes);
            let mut bitwise = buf(&bytes);
            let mut acc: u32 = 0;
            for _ in 0..n {
                acc = acc.wrapping_shl(1).wrapping_add(u32::from(bitwise.read_bool(false)));
            }
            prop_assert_eq!(whole.read_bits4(n, false), acc);
        }

        #[test]
        fn read_bits8_matches_bit_by_bit(bytes in any::<[u8; 8]>(), n in 1_usize..=64) {
            let mut whole = buf(&bytes);
            let mut bitwise = buf(&bytes);
            let mut acc: u64 = 0;
            for _ in 0..n {
                acc = acc.wrapping_shl(1).wrapping_add(u64::from(bitwise.read_bool(false)));
            }
            prop_assert_eq!(whole.read_bits8(n, false), acc);
        }

        #[test]
        fn bs_skip_bits_matches_reading_bit_by_bit(bytes in any::<[u8; 8]>(), n in 0_usize..=48) {
            // Skipping n bits leaves the cursor exactly where n single-bit reads
            // would, and subsequent reads agree.
            let mut skipped = buf(&bytes);
            let mut read = buf(&bytes);
            skipped.bs_skip_bits(n, false);
            for _ in 0..n {
                let _ = read.read_bool(false);
            }
            prop_assert_eq!(skipped.position(), read.position());
            prop_assert_eq!(skipped.read_bits8(16, false), read.read_bits8(16, false));
        }

        #[test]
        fn read_exp_decodes_encoded_value(v in 0_u32..=2_000_000) {
            // Every encoded ue(v) decodes back to v.
            let mut b = buf(&encode_ue(v));
            prop_assert_eq!(b.read_exp(false), v);
        }

        #[test]
        fn skip_exp_consumes_same_bits_as_read_exp(v in 0_u32..=2_000_000) {
            // skip_exp and read_exp advance the cursor by the same number of bits
            // (`data_bit_stream_remain` encodes the full bit offset).
            let mut skipped = buf(&encode_ue(v));
            let mut read = buf(&encode_ue(v));
            skipped.skip_exp(false);
            let _ = read.read_exp(false);
            prop_assert_eq!(skipped.data_bit_stream_remain(), read.data_bit_stream_remain());
        }

        #[test]
        fn read_exp_never_panics_on_arbitrary_input(data in any::<Vec<u8>>(), skip in any::<bool>()) {
            // Arbitrary bytes through the Exp-Golomb reader: no panic.
            let mut b = buf(&data);
            let _ = b.read_exp(skip);
            b.skip_exp(skip);
        }
    }
}
