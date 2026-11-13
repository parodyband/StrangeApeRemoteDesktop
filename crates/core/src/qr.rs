//! QR codes, for pairing a phone by scanning instead of typing.
//!
//! A small encoder (byte mode, any version, error correction level M) after
//! Project Nayuki's reference design, plus renderers: PNG for dialogs and
//! Unicode half blocks for terminals.

/// A QR code: `size` x `size` modules, dark = true.
#[derive(Clone, Debug)]
pub struct QrCode {
    pub size: usize,
    modules: Vec<bool>,
    function: Vec<bool>,
}

// Error correction level M: codewords per block and blocks, by version.
const ECC_PER_BLOCK: [u8; 41] = [
    0, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26, 26, 26, 28, 28, 28, 28, 28, 28, 28,
    28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28,
];
const NUM_BLOCKS: [u8; 41] = [
    0, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10, 10, 11, 13, 14, 16, 17, 17, 18, 20, 21, 23, 25, 26, 28, 29, 31,
    33, 35, 37, 38, 40, 43, 45, 47, 49,
];
/// Format-information bits of level M.
const ECL_M_BITS: u32 = 0;

fn raw_data_modules(ver: usize) -> usize {
    let mut r = (16 * ver + 128) * ver + 64;
    if ver >= 2 {
        let n = ver / 7 + 2;
        r -= (25 * n - 10) * n - 55;
        if ver >= 7 {
            r -= 36;
        }
    }
    r
}

fn data_codewords(ver: usize) -> usize {
    raw_data_modules(ver) / 8 - ECC_PER_BLOCK[ver] as usize * NUM_BLOCKS[ver] as usize
}

fn gf_mul(x: u8, y: u8) -> u8 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((y as u32 >> i) & 1) * x as u32;
    }
    z as u8
}

fn rs_divisor(degree: usize) -> Vec<u8> {
    let mut result = vec![0u8; degree];
    result[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            result[j] = gf_mul(result[j], root);
            if j + 1 < degree {
                result[j] ^= result[j + 1];
            }
        }
        root = gf_mul(root, 0x02);
    }
    result
}

fn rs_remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; divisor.len()];
    for &b in data {
        let factor = b ^ result.remove(0);
        result.push(0);
        for (r, &d) in result.iter_mut().zip(divisor) {
            *r ^= gf_mul(d, factor);
        }
    }
    result
}

fn alignment_positions(ver: usize, size: usize) -> Vec<usize> {
    if ver == 1 {
        return Vec::new();
    }
    let n = ver / 7 + 2;
    let step = (ver * 8 + n * 3 + 5) / (n * 4 - 4) * 2;
    let mut r: Vec<usize> = (0..n - 1).map(|i| size - 7 - i * step).collect();
    r.push(6);
    r.reverse();
    r
}

impl QrCode {
    /// Encode `data` (byte mode, level M) in the smallest version that fits.
    pub fn encode(data: &[u8]) -> Option<QrCode> {
        let ver = (1..=40).find(|&v| {
            let count_bits = if v < 10 { 8 } else { 16 };
            4 + count_bits + data.len() * 8 <= data_codewords(v) * 8 && data.len() < (1 << count_bits)
        })?;
        // Bit stream: mode, length, data, terminator, padding.
        let mut bits: Vec<bool> = Vec::new();
        let mut push = |v: u32, n: usize| (0..n).rev().for_each(|i| bits.push((v >> i) & 1 != 0));
        push(0b0100, 4);
        push(data.len() as u32, if ver < 10 { 8 } else { 16 });
        for &b in data {
            push(b as u32, 8);
        }
        let capacity = data_codewords(ver) * 8;
        let term = (capacity - bits.len()).min(4);
        bits.extend(std::iter::repeat_n(false, term));
        while !bits.len().is_multiple_of(8) {
            bits.push(false);
        }
        let mut codewords: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b as u8)).collect();
        for pad in [0xEC, 0x11].iter().cycle() {
            if codewords.len() >= capacity / 8 {
                break;
            }
            codewords.push(*pad);
        }

        let all = Self::add_ecc_and_interleave(ver, &codewords);
        let size = ver * 4 + 17;
        let mut qr = QrCode { size, modules: vec![false; size * size], function: vec![false; size * size] };
        qr.draw_function_patterns(ver);
        qr.draw_codewords(&all);
        // Pick the mask with the lowest penalty.
        let mut best = (u32::MAX, 0);
        for mask in 0..8 {
            qr.apply_mask(mask);
            qr.draw_format_bits(mask);
            let p = qr.penalty();
            if p < best.0 {
                best = (p, mask);
            }
            qr.apply_mask(mask); // undo (XOR)
        }
        qr.apply_mask(best.1);
        qr.draw_format_bits(best.1);
        Some(qr)
    }

    fn add_ecc_and_interleave(ver: usize, data: &[u8]) -> Vec<u8> {
        let num_blocks = NUM_BLOCKS[ver] as usize;
        let ecc_len = ECC_PER_BLOCK[ver] as usize;
        let raw = raw_data_modules(ver) / 8;
        let num_short = num_blocks - raw % num_blocks;
        let short_len = raw / num_blocks;
        let divisor = rs_divisor(ecc_len);
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        let mut k = 0;
        for i in 0..num_blocks {
            let dat_len = short_len - ecc_len + usize::from(i >= num_short);
            let dat = &data[k..k + dat_len];
            k += dat_len;
            let mut block = dat.to_vec();
            if i < num_short {
                block.push(0); // placeholder, skipped when interleaving
            }
            block.extend(rs_remainder(dat, &divisor));
            blocks.push(block);
        }
        let mut result = Vec::with_capacity(raw);
        for i in 0..blocks[0].len() {
            for (j, b) in blocks.iter().enumerate() {
                // Skip the placeholder of short blocks.
                if i != short_len - ecc_len || j >= num_short {
                    result.push(b[i]);
                }
            }
        }
        result
    }

    fn set_function(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * self.size + x] = dark;
        self.function[y * self.size + x] = true;
    }

    fn draw_function_patterns(&mut self, ver: usize) {
        let size = self.size;
        for i in 0..size {
            self.set_function(6, i, i % 2 == 0);
            self.set_function(i, 6, i % 2 == 0);
        }
        for (cx, cy) in [(3, 3), (size - 4, 3), (3, size - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                    if (0..size as i32).contains(&x) && (0..size as i32).contains(&y) {
                        let dist = dx.abs().max(dy.abs());
                        self.set_function(x as usize, y as usize, dist != 2 && dist != 4);
                    }
                }
            }
        }
        let pos = alignment_positions(ver, size);
        let last = pos.len().saturating_sub(1);
        for (i, &ax) in pos.iter().enumerate() {
            for (j, &ay) in pos.iter().enumerate() {
                if (i == 0 && j == 0) || (i == 0 && j == last) || (i == last && j == 0) {
                    continue;
                }
                for dy in -2i32..=2 {
                    for dx in -2i32..=2 {
                        let dark = dx.abs().max(dy.abs()) != 1;
                        self.set_function((ax as i32 + dx) as usize, (ay as i32 + dy) as usize, dark);
                    }
                }
            }
        }
        self.draw_format_bits(0); // reserve; redrawn with the real mask
        if ver >= 7 {
            let mut rem = ver as u32;
            for _ in 0..12 {
                rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
            }
            let bits = (ver as u32) << 12 | rem;
            for i in 0..18 {
                let dark = (bits >> i) & 1 != 0;
                let (a, b) = (size - 11 + i % 3, i / 3);
                self.set_function(a, b, dark);
                self.set_function(b, a, dark);
            }
        }
    }

    fn draw_format_bits(&mut self, mask: u32) {
        let data = ECL_M_BITS << 3 | mask;
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let bits = (data << 10 | rem) ^ 0x5412;
        let bit = |i: u32| (bits >> i) & 1 != 0;
        let size = self.size;
        for i in 0..=5 {
            self.set_function(8, i, bit(i as u32));
        }
        self.set_function(8, 7, bit(6));
        self.set_function(8, 8, bit(7));
        self.set_function(7, 8, bit(8));
        for i in 9..15 {
            self.set_function(14 - i, 8, bit(i as u32));
        }
        for i in 0..8 {
            self.set_function(size - 1 - i, 8, bit(i as u32));
        }
        for i in 8..15 {
            self.set_function(8, size - 15 + i, bit(i as u32));
        }
        self.set_function(8, size - 8, true);
    }

    fn draw_codewords(&mut self, data: &[u8]) {
        let size = self.size;
        let mut i = 0usize;
        let mut right = size as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..size {
                for j in 0..2 {
                    let x = (right - j) as usize;
                    let upward = (right + 1) & 2 == 0;
                    let y = if upward { size - 1 - vert } else { vert };
                    if !self.function[y * size + x] && i < data.len() * 8 {
                        self.modules[y * size + x] = (data[i >> 3] >> (7 - (i & 7))) & 1 != 0;
                        i += 1;
                    }
                }
            }
            right -= 2;
        }
    }

    fn apply_mask(&mut self, mask: u32) {
        let size = self.size;
        for y in 0..size {
            for x in 0..size {
                let invert = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                if invert && !self.function[y * size + x] {
                    self.modules[y * size + x] ^= true;
                }
            }
        }
    }

    /// The standard's penalty score (runs, 2x2 blocks, finder look-alikes,
    /// dark/light balance): lower scans more reliably.
    fn penalty(&self) -> u32 {
        let n = self.size;
        let mut p = 0u32;
        let lines = |row: bool| -> Vec<Vec<bool>> {
            (0..n).map(|a| (0..n).map(|b| if row { self.get(b, a) } else { self.get(a, b) }).collect()).collect()
        };
        for line in lines(true).into_iter().chain(lines(false)) {
            let mut run = 1;
            for i in 1..n {
                if line[i] == line[i - 1] {
                    run += 1;
                } else {
                    if run >= 5 {
                        p += 3 + (run - 5);
                    }
                    run = 1;
                }
            }
            if run >= 5 {
                p += 3 + (run - 5);
            }
            // 1:1:3:1:1 with four light modules on a side.
            let pat = [true, false, true, true, true, false, true];
            for i in 0..n.saturating_sub(6) {
                if line[i..i + 7] == pat {
                    let before = i >= 4 && line[i - 4..i].iter().all(|&d| !d);
                    let after = i + 11 <= n && line[i + 7..i + 11].iter().all(|&d| !d);
                    if before || after {
                        p += 40;
                    }
                }
            }
        }
        for y in 0..n - 1 {
            for x in 0..n - 1 {
                let c = self.get(x, y);
                if c == self.get(x + 1, y) && c == self.get(x, y + 1) && c == self.get(x + 1, y + 1) {
                    p += 3;
                }
            }
        }
        let dark = self.modules.iter().filter(|&&d| d).count();
        let total = n * n;
        let k = ((dark * 20).abs_diff(total * 10)).div_ceil(total).saturating_sub(1);
        p + k as u32 * 10
    }

    pub fn get(&self, x: usize, y: usize) -> bool {
        x < self.size && y < self.size && self.modules[y * self.size + x]
    }

    /// RGB pixels at `scale` pixels per module with a 4-module quiet zone:
    /// (width, pixels). Crisp at any size that's a multiple of the module.
    pub fn to_rgb(&self, scale: usize) -> (usize, Vec<u8>) {
        let border = 4;
        let w = (self.size + border * 2) * scale;
        let mut px = vec![255u8; w * w * 3];
        for y in 0..self.size {
            for x in 0..self.size {
                if self.get(x, y) {
                    for dy in 0..scale {
                        let row = ((y + border) * scale + dy) * w;
                        for dx in 0..scale {
                            let i = (row + (x + border) * scale + dx) * 3;
                            px[i..i + 3].fill(0);
                        }
                    }
                }
            }
        }
        (w, px)
    }

    pub fn to_png(&self, scale: usize) -> Vec<u8> {
        let (w, px) = self.to_rgb(scale);
        crate::png::encode_rgb(w as u32, w as u32, &px)
    }

    /// Text for a terminal: two modules per character (half blocks), dark on
    /// light whatever the terminal's colours, with the quiet zone.
    pub fn to_terminal(&self) -> String {
        let border = 2i32;
        let n = self.size as i32;
        let dark = |x: i32, y: i32| x >= 0 && y >= 0 && self.get(x as usize, y as usize);
        let mut out = String::new();
        let mut y = -border;
        while y < n + border {
            out += "\x1b[30;47m";
            for x in -border..n + border {
                out.push(match (dark(x, y), dark(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
            out += "\x1b[0m\n";
            y += 2;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacities_match_the_standard() {
        // Level M byte capacities (ISO/IEC 18004 table 7) for a few versions.
        let cap = |v: usize| (data_codewords(v) * 8 - 4 - if v < 10 { 8 } else { 16 }) / 8;
        assert_eq!(cap(1), 14);
        assert_eq!(cap(2), 26);
        assert_eq!(cap(5), 84);
        assert_eq!(cap(7), 122);
        assert_eq!(cap(10), 213);
        assert_eq!(cap(40), 2331);
    }

    #[test]
    fn known_reed_solomon() {
        // "HELLO WORLD" 1-M data codewords and their error correction (thonky.com tutorial).
        let data = [32, 91, 11, 120, 209, 114, 220, 77, 67, 64, 236, 17, 236, 17, 236, 17];
        let ecc = rs_remainder(&data, &rs_divisor(10));
        assert_eq!(ecc, [196, 35, 39, 119, 235, 215, 231, 226, 93, 23]);
    }

    #[test]
    fn sizes_and_finders() {
        let q = QrCode::encode(b"sard://pair?a=192.168.1.20&c=123456").unwrap();
        assert_eq!(q.size, 29); // version 3
        for (cx, cy) in [(0, 0), (q.size - 7, 0), (0, q.size - 7)] {
            assert!(q.get(cx, cy) && q.get(cx + 6, cy + 6) && !q.get(cx + 1, cy + 1) && q.get(cx + 3, cy + 3));
        }
        assert!(QrCode::encode(&[b'x'; 3000]).is_none());
        assert!(q.to_terminal().contains('█'));
    }
}
