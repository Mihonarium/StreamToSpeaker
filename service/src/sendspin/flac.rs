//! Minimal streaming FLAC encoder for Sendspin players that accept FLAC
//! but not raw PCM.
//!
//! Each call encodes one block into one complete FLAC frame (Sendspin
//! chunks must carry whole codec units). Subframes are CONSTANT (digital
//! silence costs a few bytes), FIXED order 0–4 with Rice-coded residuals,
//! or VERBATIM — whichever is smallest. No LPC: ample for a LAN stream,
//! and every FLAC decoder handles these subframe types. The codec header
//! is `fLaC` + the STREAMINFO block, as the protocol requires.

/// Bit writer, MSB first.
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn new() -> Self {
        Self { out: Vec::with_capacity(4096), acc: 0, n: 0 }
    }

    fn put(&mut self, value: u64, bits: u32) {
        debug_assert!(bits <= 32);
        if bits == 0 {
            return;
        }
        self.acc = (self.acc << bits) | (value & ((1u64 << bits) - 1));
        self.n += bits;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
    }

    fn put_signed(&mut self, value: i64, bits: u32) {
        self.put(value as u64 & ((1u64 << bits) - 1), bits);
    }

    fn unary_zeros_then_one(&mut self, zeros: u64) {
        let mut z = zeros;
        while z >= 32 {
            self.put(0, 32);
            z -= 32;
        }
        self.put(1, z as u32 + 1);
    }

    fn align(&mut self) {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put(0, pad);
        }
    }
}

fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

pub struct FlacEncoder {
    sample_rate: u32,
    channels: u16,
    bits: u16,
    block_size: u16,
    frame_number: u64,
}

impl FlacEncoder {
    /// `block_size` frames per FLAC frame (= per Sendspin chunk).
    pub fn new(sample_rate: u32, channels: u16, bits: u16, block_size: u16) -> Self {
        Self {
            sample_rate,
            channels,
            bits,
            block_size,
            frame_number: 0,
        }
    }

    /// `fLaC` + STREAMINFO (last metadata block).
    pub fn codec_header(&self) -> Vec<u8> {
        let mut b = Bits::new();
        b.out.extend_from_slice(b"fLaC");
        b.put(1, 1); // last block
        b.put(0, 7); // STREAMINFO
        b.put(34, 24);
        b.put(self.block_size as u64, 16); // min block size
        b.put(self.block_size as u64, 16); // max block size
        b.put(0, 24); // min frame size unknown
        b.put(0, 24); // max frame size unknown
        b.put(self.sample_rate as u64, 20);
        b.put(self.channels as u64 - 1, 3);
        b.put(self.bits as u64 - 1, 5);
        b.put(0, 4); // total samples unknown (stream): 36 bits
        b.put(0, 32);
        for _ in 0..4 {
            b.put(0, 32); // MD5 unknown
        }
        b.out
    }

    /// Encode one block of interleaved samples (`frames * channels`,
    /// values within `bits`). The final block of a stream may be short.
    pub fn encode(&mut self, interleaved: &[i32]) -> Vec<u8> {
        let ch = self.channels as usize;
        let frames = interleaved.len() / ch;
        let mut b = Bits::new();
        // --- frame header ---
        b.put(0b1111_1111_1111_10, 14);
        b.put(0, 1); // reserved
        b.put(0, 1); // fixed blocking strategy
        b.put(0b0111, 4); // block size: 16-bit (n-1) at end of header
        let rate_code = match self.sample_rate {
            88_200 => 0b0001,
            176_400 => 0b0010,
            192_000 => 0b0011,
            8_000 => 0b0100,
            16_000 => 0b0101,
            22_050 => 0b0110,
            24_000 => 0b0111,
            32_000 => 0b1000,
            44_100 => 0b1001,
            48_000 => 0b1010,
            96_000 => 0b1011,
            _ => 0b0000, // from STREAMINFO
        };
        b.put(rate_code, 4);
        b.put(ch as u64 - 1, 4); // independent channels
        let size_code = match self.bits {
            8 => 0b001,
            12 => 0b010,
            16 => 0b100,
            20 => 0b101,
            24 => 0b110,
            32 => 0b111,
            _ => 0b000,
        };
        b.put(size_code, 3);
        b.put(0, 1); // reserved
        put_utf8_number(&mut b, self.frame_number);
        b.put(frames as u64 - 1, 16);
        let header_crc = crc8(&b.out);
        b.put(header_crc as u64, 8);
        // --- subframes ---
        let mut chan = vec![0i64; frames];
        for c in 0..ch {
            for (i, v) in chan.iter_mut().enumerate() {
                *v = interleaved[i * ch + c] as i64;
            }
            write_subframe(&mut b, &chan, self.bits as u32);
        }
        b.align();
        let crc = crc16(&b.out);
        b.put(crc as u64, 16);
        self.frame_number += 1;
        b.out
    }
}

/// FLAC's UTF-8-style variable-length frame number.
fn put_utf8_number(b: &mut Bits, n: u64) {
    if n < 0x80 {
        b.put(n, 8);
        return;
    }
    let mut bytes = Vec::new();
    let mut v = n;
    let mut cont = 0;
    while v >= (1 << (6 - cont)) {
        bytes.push(0x80 | (v & 0x3F) as u8);
        v >>= 6;
        cont += 1;
    }
    let lead_mask: u8 = !(0xFFu8 >> (cont + 1));
    b.put((lead_mask | v as u8) as u64, 8);
    for byte in bytes.iter().rev() {
        b.put(*byte as u64, 8);
    }
}

fn fixed_residual(x: &[i64], order: usize) -> Vec<i64> {
    (order..x.len())
        .map(|i| match order {
            0 => x[i],
            1 => x[i] - x[i - 1],
            2 => x[i] - 2 * x[i - 1] + x[i - 2],
            3 => x[i] - 3 * x[i - 1] + 3 * x[i - 2] - x[i - 3],
            _ => x[i] - 4 * x[i - 1] + 6 * x[i - 2] - 4 * x[i - 3] + x[i - 4],
        })
        .collect()
}

fn zigzag(r: i64) -> u64 {
    ((r << 1) ^ (r >> 63)) as u64
}

/// Best Rice parameter and the resulting bit count for one partition.
fn rice_cost(res: &[i64]) -> (u32, u64) {
    let mut best = (0u32, u64::MAX);
    for k in 0..=14u32 {
        let bits: u64 = res.iter().map(|&r| (zigzag(r) >> k) + 1 + k as u64).sum();
        if bits < best.1 {
            best = (k, bits);
        }
    }
    best
}

fn write_subframe(b: &mut Bits, x: &[i64], bps: u32) {
    if x.iter().all(|&v| v == x[0]) {
        b.put(0, 1);
        b.put(0b000000, 6); // CONSTANT
        b.put(0, 1);
        b.put_signed(x[0], bps);
        return;
    }
    let verbatim_bits = x.len() as u64 * bps as u64;
    let mut best: Option<(usize, u32, u64, Vec<i64>)> = None;
    for order in 0..=4usize.min(x.len().saturating_sub(1)) {
        let res = fixed_residual(x, order);
        // Residuals must fit the 32-bit escape-free Rice path.
        if res.iter().any(|r| r.unsigned_abs() > (1u64 << 30)) {
            continue;
        }
        let (k, bits) = rice_cost(&res);
        let total = bits + order as u64 * bps as u64 + 2 + 4 + 4;
        if best.as_ref().map(|b| total < b.2).unwrap_or(true) {
            best = Some((order, k, total, res));
        }
    }
    match best {
        Some((order, k, total, res)) if total < verbatim_bits => {
            b.put(0, 1);
            b.put(0b001000 | order as u64, 6); // FIXED
            b.put(0, 1);
            for &w in &x[..order] {
                b.put_signed(w, bps);
            }
            b.put(0b00, 2); // Rice, 4-bit parameters
            b.put(0, 4); // partition order 0
            b.put(k as u64, 4);
            for &r in &res {
                let u = zigzag(r);
                b.unary_zeros_then_one(u >> k);
                b.put(u & ((1u64 << k) - 1), k);
            }
        }
        _ => {
            b.put(0, 1);
            b.put(0b000001, 6); // VERBATIM
            b.put(0, 1);
            for &v in x {
                b.put_signed(v, bps);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaminfo_layout() {
        let e = FlacEncoder::new(48_000, 2, 16, 960);
        let h = e.codec_header();
        assert_eq!(&h[..4], b"fLaC");
        assert_eq!(h[4], 0x80); // last block, STREAMINFO
        assert_eq!(u32::from_be_bytes([0, h[5], h[6], h[7]]), 34);
        assert_eq!(h.len(), 4 + 4 + 34);
        assert_eq!(u16::from_be_bytes([h[8], h[9]]), 960);
        // sample rate (20 bits) starts at byte 18.
        let sr = ((h[18] as u32) << 12) | ((h[19] as u32) << 4) | ((h[20] as u32) >> 4);
        assert_eq!(sr, 48_000);
    }

    #[test]
    fn crcs_match_known_values() {
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(crc16(b"123456789"), 0xFEE8);
    }

    #[test]
    fn frames_start_with_sync_and_silence_is_tiny() {
        let mut e = FlacEncoder::new(44_100, 2, 16, 882);
        let silence = vec![0i32; 882 * 2];
        let f = e.encode(&silence);
        assert_eq!(f[0], 0xFF);
        assert_eq!(f[1], 0xF8);
        assert!(f.len() < 32, "silence frame {} bytes", f.len());
        let tone: Vec<i32> = (0..882)
            .flat_map(|i| {
                let s = ((i as f64 * 0.06).sin() * 10_000.0) as i32;
                [s, s]
            })
            .collect();
        let f2 = e.encode(&tone);
        assert!(f2.len() < 882 * 4, "tone frame {} bytes not compressed", f2.len());
    }

    #[test]
    fn utf8_frame_numbers() {
        let mut b = Bits::new();
        put_utf8_number(&mut b, 0x7F);
        assert_eq!(b.out, vec![0x7F]);
        let mut b = Bits::new();
        put_utf8_number(&mut b, 0x80);
        assert_eq!(b.out, vec![0xC2, 0x80]);
        let mut b = Bits::new();
        put_utf8_number(&mut b, 0x800);
        assert_eq!(b.out, vec![0xE0, 0xA0, 0x80]);
    }
}
