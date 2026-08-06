//! Build "uncompressed-ALAC" frames from 16-bit stereo PCM.
//!
//! AirPlay 1 frames its audio as ALAC, but the codec supports a
//! pass-through escape that lets a sender ship raw PCM samples wrapped
//! in a stub ALAC bitstream without actually running the compressor.
//!
//! ⚠️ **Debug/fallback path only.** An earlier version of this comment
//! claimed iTunes sends this escape — packet capture disproved that:
//! iTunes 12.13.10 sends *real compressed* ALAC (payloads 311–999 B
//! variable, vs the escape's constant 1416 B), and so do OwnTone,
//! AirConnect/libraop and node_airtunes2. The only field sender using
//! the escape is PipeWire `module-raop-sink`, and it reproduces the
//! connected-but-silent symptom on Sonos. The default RTP path encodes
//! with Apple's real encoder (`alac-encoder` crate); this module stays
//! for the `airplay_uncompressed_alac` config escape hatch and for
//! shairport-class receivers where the escape is known-good.
//!
//! The bit layout below is taken verbatim from PipeWire's
//! `write_codec_pcm()` (its `module-raop-sink.c`) and cross-checked
//! against the openairplay reference. Each frame is a 55-bit header
//! followed by interleaved big-endian 16-bit samples and a 3-bit
//! end-of-element tag, padded out to a whole byte.
//!
//! ## Header (55 bits)
//!
//! | Bits | Value          | Meaning                                       |
//! |------|----------------|-----------------------------------------------|
//! |   3  | `001`          | Element type: CPE (Channel Pair Element, stereo) |
//! |   4  | `0000`         | Instance tag (always 0)                       |
//! |   8  | `0x00`         | Header byte: "Unknown" continuation           |
//! |   4  | `0000`         | More header padding                           |
//! |   1  | `1`            | `hassize` — frame length follows              |
//! |   2  | `00`           | Unused                                        |
//! |   1  | `1`            | `is-not-compressed` — escape: raw PCM follows |
//! |  32  | `n_frames`     | Frame count, BE u32                           |
//!
//! Then `n_frames * 2 * 16` bits of stereo audio (sample order
//! `L_hi, L_lo, R_hi, R_lo` per source frame), then `0b111` end tag,
//! then 0..7 bits of zero pad up to a whole byte.
//!
//! For our canonical 352-frame stereo packet:
//!   * Header:       55 bits
//!   * Audio:    11264 bits  (352 × 2 × 16)
//!   * End tag:      3 bits
//!   * Total:    11322 bits = 1415.25 bytes → padded to **1416 bytes**.

/// Build a single ALAC frame containing `frames` stereo sample pairs.
/// `samples_le` is `[L0, R0, L1, R1, ...]` interleaved i16 in host
/// (little-endian on Windows) order — we re-emit as big-endian inside
/// the bitstream.
///
/// Returns a fresh `Vec<u8>` of the bit-packed frame, byte-aligned.
pub fn build_uncompressed_alac_frame(samples_le: &[i16]) -> Vec<u8> {
    debug_assert!(samples_le.len() % 2 == 0, "stereo frame count must be even");
    let n_frames = (samples_le.len() / 2) as u32;

    // Pre-size: header(55) + samples(n*32) + end(3), rounded up.
    let bit_len: u64 = 55 + (n_frames as u64) * 32 + 3;
    let byte_len = ((bit_len + 7) / 8) as usize;
    let mut out = vec![0u8; byte_len];
    let mut bw = BitWriter::new(&mut out);

    // 55-bit fixed header for "stereo, hassize, uncompressed".
    bw.write_bits(0b001, 3); // CPE
    bw.write_bits(0, 4); // instance tag
    bw.write_bits(0, 8); // unknown continuation
    bw.write_bits(0, 4); // unknown
    bw.write_bits(1, 1); // hassize
    bw.write_bits(0, 2); // unused
    bw.write_bits(1, 1); // is-not-compressed (escape)
    // 32-bit frame count, BE byte order.
    bw.write_bits((n_frames >> 24) & 0xff, 8);
    bw.write_bits((n_frames >> 16) & 0xff, 8);
    bw.write_bits((n_frames >> 8) & 0xff, 8);
    bw.write_bits(n_frames & 0xff, 8);

    // PCM samples — big-endian bytes per channel.
    for &s in samples_le {
        let be = s.to_be_bytes();
        bw.write_bits(be[0] as u32, 8);
        bw.write_bits(be[1] as u32, 8);
    }

    // End-of-element marker.
    bw.write_bits(0b111, 3);

    out
}

/// Minimal MSB-first bit writer over a pre-sized byte buffer. Bits
/// beyond `buf.len() * 8` are silently dropped (caller is responsible
/// for sizing correctly).
struct BitWriter<'a> {
    buf: &'a mut [u8],
    bit_pos: usize,
}

impl<'a> BitWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, bit_pos: 0 }
    }

    fn write_bits(&mut self, value: u32, num_bits: u32) {
        debug_assert!(num_bits <= 32);
        let mut remaining = num_bits;
        while remaining > 0 {
            let byte_idx = self.bit_pos / 8;
            if byte_idx >= self.buf.len() {
                return;
            }
            let bit_offset_in_byte = self.bit_pos % 8;
            let space_in_byte = 8 - bit_offset_in_byte;
            let bits_now = remaining.min(space_in_byte as u32);

            let shift = remaining - bits_now;
            let chunk = ((value >> shift) & ((1u32 << bits_now) - 1)) as u8;
            let dest_shift = (space_in_byte as u32 - bits_now) as u8;
            self.buf[byte_idx] |= chunk << dest_shift;

            self.bit_pos += bits_now as usize;
            remaining -= bits_now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_352_frame_is_1416_bytes() {
        // The canonical RAOP frame size — every implementation produces
        // exactly 1416 bytes for 352 stereo 16-bit samples.
        let samples = vec![0i16; 352 * 2];
        let frame = build_uncompressed_alac_frame(&samples);
        assert_eq!(frame.len(), 1416);
    }

    #[test]
    fn empty_frame_has_no_samples_just_header_and_tag() {
        // 55-bit header + 3-bit end tag = 58 bits = 7.25 bytes → 8 bytes.
        let frame = build_uncompressed_alac_frame(&[]);
        assert_eq!(frame.len(), 8);
        // First byte: top 3 bits = `001` (CPE), next 4 = 0000, then 1 bit
        // of the 8-bit "unknown" zero block.
        //   001_0000_0  = 0010_0000 = 0x20
        assert_eq!(frame[0], 0x20);
    }

    #[test]
    fn n_frames_field_is_encoded_big_endian() {
        // Pass exactly 0x01_02_03_04 frames (16,909,060) — not realistic
        // but unambiguously verifies the 32-bit field order.
        //
        // The header bits before n_frames sum to 23, so n_frames begins
        // at bit 23. Bit 23 is byte 2's bit-0 (MSB-counted) which is
        // bit-7 in MSB-first writing. Then it spans bytes 2..6 with the
        // 8-bit byte values shifted into byte 6's MSB.
        //
        // Concretely with n_frames = 0x01020304:
        //   byte 2 bit 0..6 = previous 7 bits of header (zero pad),
        //   byte 2 bit 7    = top bit of 0x01 (0)
        //   byte 3          = next 8 bits = 0x02
        //   ...
        // For a robust check, just round-trip the build and read back
        // the 32-bit field from where we know it ends up.
        let mut samples = Vec::new();
        samples.resize(0x10 * 2, 0); // arbitrary, won't be 0x01020304
        let frame = build_uncompressed_alac_frame(&samples);
        // The first sample's MSB sits at bit 55; bytes 0..6 contain header
        // bits 0..55. The n_frames field occupies bits 23..55. Extract:
        let mut bits = 0u64;
        for b in &frame[0..7] {
            bits = (bits << 8) | (*b as u64);
        }
        // bits now holds bits 0..56, MSB-first. n_frames is bits 23..55:
        let n = ((bits >> (56 - 55)) & 0xFFFF_FFFF) as u32;
        assert_eq!(n, 0x10);
    }
}
