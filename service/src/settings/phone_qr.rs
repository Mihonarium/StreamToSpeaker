//! A version 5-L QR symbol in byte mode. Its single RS block holds up to
//! 106 bytes, enough for the longest numeric IPv6 phone URL with a port.
use egui::{Color32, Rect, Sense, Ui};

const SIDE: usize = 37;

fn multiply(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        b >>= 1;
        let high = a & 0x80 != 0;
        a <<= 1;
        if high {
            a ^= 0x1d;
        }
    }
    product
}

fn codewords(bytes: &[u8]) -> Vec<u8> {
    let mut data = vec![0x40 | (bytes.len() >> 4) as u8];
    let mut carry = (bytes.len() as u8) << 4;
    for &byte in bytes {
        data.push(carry | byte >> 4);
        carry = byte << 4;
    }
    data.push(carry); // Four zero terminator bits.
    let fill = 108 - data.len();
    data.extend([0xec, 0x11].into_iter().cycle().take(fill));
    let mut divisor = [0u8; 26];
    divisor[25] = 1;
    let mut root = 1;
    for _ in 0..26 {
        for j in 0..26 {
            divisor[j] = multiply(divisor[j], root) ^ divisor.get(j + 1).copied().unwrap_or(0);
        }
        root = multiply(root, 2);
    }
    let mut remainder = [0u8; 26];
    for b in &data {
        let factor = *b ^ remainder[0];
        remainder.rotate_left(1);
        remainder[25] = 0;
        for (r, d) in remainder.iter_mut().zip(divisor) {
            *r ^= multiply(d, factor);
        }
    }
    data.extend(remainder);
    data
}

fn symbol(bytes: &[u8]) -> [[Option<bool>; SIDE]; SIDE] {
    let mut grid = [[None; SIDE]; SIDE];
    for i in 0..SIDE {
        grid[6][i] = Some(i % 2 == 0);
        grid[i][6] = Some(i % 2 == 0);
    }
    for (cx, cy) in [(3, 3), (SIDE - 4, 3), (3, SIDE - 4), (30, 30)] {
        let alignment = cx == 30;
        let radius = if alignment { 2 } else { 4 };
        for (y, row) in grid.iter_mut().enumerate() {
            for (x, cell) in row.iter_mut().enumerate() {
                let d = x.abs_diff(cx).max(y.abs_diff(cy));
                if d <= radius {
                    *cell = Some(if alignment { d != 1 } else { d != 2 && d != 4 });
                }
            }
        }
    }
    // Format bits for level L with mask 0, including BCH and the format mask.
    let bit = |i| Some(0x77c4u16 & (1u16 << i) != 0);
    for i in 0..6 {
        grid[i][8] = bit(i);
    }
    grid[7][8] = bit(6);
    grid[8][8] = bit(7);
    grid[8][7] = bit(8);
    for i in 9..15 {
        grid[8][14 - i] = bit(i);
    }
    for i in 0..8 {
        grid[8][SIDE - 1 - i] = bit(i);
    }
    for i in 8..15 {
        grid[SIDE - 15 + i][8] = bit(i);
    }
    grid[SIDE - 8][8] = Some(true);
    let data = codewords(bytes);
    let mut index = 0;
    let mut right = SIDE - 1;
    let mut up = true;
    while right > 0 {
        if right == 6 {
            right -= 1;
        }
        for i in 0..SIDE {
            let y = if up { SIDE - 1 - i } else { i };
            for x in [right, right - 1] {
                if grid[y][x].is_none() {
                    let dark = data
                        .get(index / 8)
                        .is_some_and(|b| b & (0x80 >> (index % 8)) != 0);
                    grid[y][x] = Some(dark ^ ((x + y) % 2 == 0));
                    index += 1;
                }
            }
        }
        up = !up;
        right = right.saturating_sub(2);
    }
    grid
}

pub fn show(ui: &mut Ui, link: &str) {
    if link.len() > 106 {
        return;
    }
    let grid = symbol(link.as_bytes());
    let unit = 3.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::Vec2::splat((SIDE + 8) as f32 * unit), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, Color32::WHITE);
    for (y, row) in grid.iter().enumerate() {
        for (x, dark) in row.iter().enumerate() {
            if *dark == Some(true) {
                let pos = rect.min + egui::vec2((x + 4) as f32, (y + 4) as f32) * unit;
                ui.painter().rect_filled(
                    ui.painter()
                        .round_rect_to_pixels(Rect::from_min_size(pos, egui::Vec2::splat(unit))),
                    0.0,
                    Color32::BLACK,
                );
            }
        }
    }
}
