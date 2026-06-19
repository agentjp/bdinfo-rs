#![no_main]
//! Fuzz target: the MSB-first bitstream reader fed arbitrary bytes —
//! `bdinfo_rs_core::bitstream::TsStreamBuffer`. The transport-stream payload behind
//! every codec is attacker-controlled, so this drives the whole reader surface
//! (`read_bool`, `read_bits2`/`4`/`8`, Exp-Golomb, the `bs_skip_*` helpers, seek)
//! over the fuzzer's bytes and asserts no panic / no out-of-bounds read. The
//! `*_never_panic*` / `*_matches_*` proptests cover this on Windows; this is the
//! adversarial amplifier on nightly/Linux — see fuzz/README.md.

use bdinfo_rs_core::bitstream::{SeekOrigin, TsStreamBuffer};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut b = TsStreamBuffer::new();
    b.add(data, 0, data.len());
    b.begin_read();

    // Each byte is an opcode: the low bit toggles H.26x emulation-prevention, the
    // next three bits pick a reader, and the remaining bits size the read. The
    // fuzzer thus steers the operation sequence with the same bytes it reads.
    for &op in data {
        let skip = op & 1 == 1;
        let arg = usize::from(op >> 1);
        match (op >> 1) & 0b111 {
            0 => {
                let _ = b.read_bool(skip);
            }
            1 => {
                let _ = b.read_bits2(arg % 17, skip);
            }
            2 => {
                let _ = b.read_bits4(arg % 33, skip);
            }
            3 => {
                let _ = b.read_bits8(arg % 65, skip);
            }
            4 => {
                let _ = b.read_exp(skip);
            }
            5 => b.skip_exp(skip),
            6 => b.bs_skip_bits(arg % 40, skip),
            _ => b.bs_skip_bytes(i32::from(op) - 4, skip),
        }
    }

    // Second pass: seek (all three origins) plus the remaining skip helpers and
    // every getter, so reads after a jump — including past-content / past-window
    // ones — are exercised too.
    b.begin_read();
    let _ = b.read_bool(false);
    b.bs_skip_next_byte();
    b.bs_reset_bits();
    b.skip_exp_multi(3, true);
    b.seek(2, SeekOrigin::Begin);
    b.seek(1, SeekOrigin::Current);
    b.seek(-1, SeekOrigin::End);
    let _ = b.read_bits8(40, true);
    let _ = (b.position(), b.length(), b.transfer_length());
    let _ = (b.data_bit_stream_remain(), b.data_bit_stream_remain_bytes());
});
