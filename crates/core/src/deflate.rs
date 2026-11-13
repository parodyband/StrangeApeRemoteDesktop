//! zlib streams (RFC 1950/1951) for PNG: a decoder for every block type,
//! and a small encoder (greedy LZ77, fixed Huffman codes) that shrinks
//! screenshots well enough. No dependencies.

const LBASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DEXT: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    (b << 16) | a
}

// ---- decoding ----

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    count: u32,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        while self.count < n {
            self.buf |= (*self.data.get(self.pos)? as u64) << self.count;
            self.pos += 1;
            self.count += 8;
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.count -= n;
        Some(v)
    }

    fn align(&mut self) {
        let drop = self.count % 8;
        self.buf >>= drop;
        self.count -= drop;
    }
}

/// Canonical Huffman code: codes per length, and symbols in code order.
struct Huffman {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Option<Huffman> {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[l as usize] += 1;
        }
        // Over-subscribed codes are invalid; incomplete ones are allowed.
        let mut left = 1i32;
        for &c in &count[1..] {
            left = (left << 1) - c as i32;
            if left < 0 {
                return None;
            }
        }
        let mut offs = [0u16; 16];
        for l in 1..15 {
            offs[l + 1] = offs[l] + count[l];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        count[0] = 0;
        Some(Huffman { count, symbol })
    }

    fn decode(&self, b: &mut Bits) -> Option<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.bits(1)? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                return self.symbol.get((index + code - first) as usize).copied();
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

fn fixed_tables() -> (Huffman, Huffman) {
    let mut l = [0u8; 288];
    l[..144].fill(8);
    l[144..256].fill(9);
    l[256..280].fill(7);
    l[280..].fill(8);
    (Huffman::new(&l).unwrap(), Huffman::new(&[5; 30]).unwrap())
}

fn dynamic_tables(b: &mut Bits) -> Option<(Huffman, Huffman)> {
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let nlen = b.bits(5)? as usize + 257;
    let ndist = b.bits(5)? as usize + 1;
    let ncode = b.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return None;
    }
    let mut cl = [0u8; 19];
    for &i in &ORDER[..ncode] {
        cl[i] = b.bits(3)? as u8;
    }
    let ch = Huffman::new(&cl)?;
    let mut lengths = vec![0u8; nlen + ndist];
    let mut i = 0;
    while i < lengths.len() {
        let sym = ch.decode(b)?;
        let (value, repeat) = match sym {
            0..=15 => (sym as u8, 1),
            16 => (*lengths.get(i.checked_sub(1)?)?, 3 + b.bits(2)? as usize),
            17 => (0, 3 + b.bits(3)? as usize),
            _ => (0, 11 + b.bits(7)? as usize),
        };
        if i + repeat > lengths.len() {
            return None;
        }
        lengths[i..i + repeat].fill(value);
        i += repeat;
    }
    if lengths[256] == 0 {
        return None;
    }
    Some((Huffman::new(&lengths[..nlen])?, Huffman::new(&lengths[nlen..])?))
}

/// Raw deflate data → bytes, or None if it's malformed or would exceed `limit`.
pub fn inflate(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut b = Bits { data, pos: 0, buf: 0, count: 0 };
    let mut out = Vec::new();
    loop {
        let last = b.bits(1)? == 1;
        match b.bits(2)? {
            0 => {
                b.align();
                let len = b.bits(16)? as usize;
                if b.bits(16)? as usize != !len & 0xffff || out.len() + len > limit {
                    return None;
                }
                // Byte-aligned and the bit buffer is empty: copy straight through.
                debug_assert_eq!(b.count, 0);
                out.extend_from_slice(data.get(b.pos..b.pos + len)?);
                b.pos += len;
            }
            t @ (1 | 2) => {
                let (lit, dist) = if t == 1 { fixed_tables() } else { dynamic_tables(&mut b)? };
                loop {
                    let sym = lit.decode(&mut b)? as usize;
                    if sym < 256 {
                        if out.len() >= limit {
                            return None;
                        }
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let i = sym - 257;
                        let len = *LBASE.get(i)? as usize + b.bits(LEXT[i] as u32)? as usize;
                        let d = dist.decode(&mut b)? as usize;
                        let d = *DBASE.get(d)? as usize + b.bits(*DEXT.get(d)? as u32)? as usize;
                        if d > out.len() || out.len() + len > limit {
                            return None;
                        }
                        let start = out.len() - d;
                        for k in 0..len {
                            out.push(out[start + k]);
                        }
                    }
                }
            }
            _ => return None,
        }
        if last {
            return Some(out);
        }
    }
}

/// zlib stream → bytes (checks the header and checksum).
pub fn zlib_decompress(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    let (&cmf, &flg) = (data.first()?, data.get(1)?);
    if cmf & 0x0f != 8 || !((cmf as u16) << 8 | flg as u16).is_multiple_of(31) || flg & 0x20 != 0 || data.len() < 6 {
        return None;
    }
    let out = inflate(&data[2..data.len() - 4], limit)?;
    let sum = u32::from_be_bytes(data[data.len() - 4..].try_into().ok()?);
    (adler32(&out) == sum).then_some(out)
}

// ---- encoding ----

struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    count: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, n: u32) {
        self.buf |= (v as u64) << self.count;
        self.count += n;
        while self.count >= 8 {
            self.out.push(self.buf as u8);
            self.buf >>= 8;
            self.count -= 8;
        }
    }

    /// A Huffman code (sent most significant bit first).
    fn code(&mut self, code: u32, n: u32) {
        self.put(code.reverse_bits() >> (32 - n), n);
    }

    fn literal(&mut self, sym: u32) {
        match sym {
            0..=143 => self.code(0x30 + sym, 8),
            144..=255 => self.code(0x190 + sym - 144, 9),
            256..=279 => self.code(sym - 256, 7),
            _ => self.code(0xc0 + sym - 280, 8),
        }
    }

    fn matched(&mut self, len: usize, dist: usize) {
        let i = LBASE.iter().rposition(|&b| b as usize <= len).unwrap();
        self.literal(257 + i as u32);
        self.put((len - LBASE[i] as usize) as u32, LEXT[i] as u32);
        let j = DBASE.iter().rposition(|&b| b as usize <= dist).unwrap();
        self.code(j as u32, 5);
        self.put((dist - DBASE[j] as usize) as u32, DEXT[j] as u32);
    }
}

/// Bytes → raw deflate data (one fixed-Huffman block).
pub fn deflate(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 32 * 1024;
    const HASH_BITS: u32 = 15;
    const CHAIN: usize = 24;
    let hash = |i: usize| {
        let v = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], 0]);
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    };
    // head: newest position + 1 per hash (0 = none); prev: the one before, per window slot.
    let mut head = vec![0u32; 1 << HASH_BITS];
    let mut prev = vec![0u32; WINDOW];
    let mut w = BitWriter { out: Vec::with_capacity(data.len() / 4 + 64), buf: 0, count: 0 };
    w.put(1, 1); // final block
    w.put(1, 2); // fixed Huffman codes
    let mut i = 0;
    let insert = |head: &mut [u32], prev: &mut [u32], i: usize| {
        if i + 2 < data.len() {
            let h = hash(i);
            prev[i % WINDOW] = head[h];
            head[h] = i as u32 + 1;
        }
    };
    while i < data.len() {
        let (mut best_len, mut best_dist) = (0, 0);
        if i + 2 < data.len() {
            let max = (data.len() - i).min(258);
            let mut cand = head[hash(i)] as usize;
            let mut steps = 0;
            while cand > 0 && steps < CHAIN {
                let p = cand - 1;
                if i - p > WINDOW - 1 {
                    break;
                }
                if data[p + best_len.min(max - 1)] == data[i + best_len.min(max - 1)] {
                    let l = data[p..p + max].iter().zip(&data[i..i + max]).take_while(|(a, b)| a == b).count();
                    if l > best_len {
                        (best_len, best_dist) = (l, i - p);
                        if l == max {
                            break;
                        }
                    }
                }
                let next = prev[p % WINDOW] as usize;
                if next >= cand {
                    break;
                }
                cand = next;
                steps += 1;
            }
        }
        if best_len >= 3 {
            w.matched(best_len, best_dist);
            for k in i..i + best_len {
                insert(&mut head, &mut prev, k);
            }
            i += best_len;
        } else {
            w.literal(data[i] as u32);
            insert(&mut head, &mut prev, i);
            i += 1;
        }
    }
    w.literal(256);
    w.put(0, 7); // flush the last byte
    w.out
}

/// Bytes → zlib stream.
pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend(deflate(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rand::XorShift;

    fn round_trip(data: &[u8]) {
        let z = zlib_compress(data);
        assert_eq!(zlib_decompress(&z, data.len()).as_deref(), Some(data), "len {}", data.len());
    }

    #[test]
    fn round_trips() {
        round_trip(b"");
        round_trip(b"a");
        round_trip(b"abcabcabcabcabcabcabcabcabcabcabcabc");
        round_trip(&[7u8; 100_000]);
        let mut r = XorShift(42);
        let noise: Vec<u8> = (0..70_000).map(|_| r.next_u64() as u8).collect();
        round_trip(&noise);
        // Screenshot-like: flat runs and repeated rows.
        let mut img = Vec::new();
        for y in 0..300u32 {
            for x in 0..400u32 {
                img.extend_from_slice(&[(x / 50) as u8 * 30, (y / 20) as u8, 200, 255]);
            }
        }
        round_trip(&img);
        assert!(zlib_compress(&img).len() < img.len() / 20, "compresses flat images");
    }

    #[test]
    fn limit_and_garbage() {
        let z = zlib_compress(&[1u8; 1000]);
        assert_eq!(zlib_decompress(&z, 999), None);
        let mut bad = z.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert_eq!(zlib_decompress(&bad, 1000), None, "checksum");
        let mut r = XorShift(7);
        for _ in 0..2000 {
            let junk: Vec<u8> = (0..r.below(64)).map(|_| r.next_u64() as u8).collect();
            let _ = inflate(&junk, 1 << 16);
        }
    }

    /// Dynamic-Huffman output from real zlib (Python `zlib.compress(d, 9)`).
    #[test]
    fn decodes_zlib_output() {
        let z = [
            0x78, 0xda, 0xbd, 0x94, 0xc9, 0x11, 0x02, 0x21, 0x10, 0x45, 0xef, 0x46, 0xd1, 0x21, 0x08, 0x0c, 0xd0, 0x98,
            0x8d, 0x0b, 0xea, 0x28, 0x0e, 0x3a, 0x3a, 0x6e, 0xd1, 0x5b, 0x9a, 0x81, 0xef, 0xc0, 0x99, 0xfa, 0xd5, 0x9f,
            0xbf, 0x95, 0x7e, 0xc8, 0x32, 0x5f, 0xc8, 0x6d, 0x9f, 0xe5, 0x32, 0xf5, 0xeb, 0xa3, 0xac, 0xc6, 0xfa, 0x18,
            0x64, 0x5b, 0x9f, 0x72, 0x98, 0x4e, 0xe7, 0xab, 0xd4, 0x7b, 0x1e, 0x7f, 0xcf, 0x65, 0xf9, 0x7e, 0xc9, 0xa6,
            0xee, 0x66, 0xe5, 0x8b, 0x31, 0x00, 0xd3, 0x01, 0x4c, 0x22, 0xdc, 0x02, 0x00, 0x59, 0x0f, 0x40, 0x8e, 0x5c,
            0xea, 0xc8, 0x9f, 0x02, 0x11, 0x4f, 0x89, 0x4b, 0x8e, 0x88, 0x47, 0xd8, 0x75, 0x11, 0x80, 0xa2, 0x25, 0xf4,
            0x88, 0x0c, 0x44, 0xbb, 0x40, 0x2e, 0x25, 0x94, 0x3c, 0x62, 0x53, 0x44, 0x4d, 0x27, 0x7f, 0xf2, 0x84, 0x5e,
            0x42, 0x6d, 0x42, 0xab, 0x82, 0xe6, 0x0b, 0xfd, 0x89, 0xd0, 0xf3, 0xc4, 0x27, 0x25, 0x79, 0x25, 0xd1, 0xb3,
            0xa4, 0xb7, 0x4a, 0xe8, 0x79, 0x22, 0x9e, 0x25, 0x79, 0x55, 0xb4, 0xc9, 0x68, 0x5e, 0x89, 0xe4, 0x86, 0x5c,
            0x52, 0x52, 0xa7, 0x80, 0x3a, 0x48, 0xcc, 0x75, 0xc4, 0x27, 0x83, 0x52, 0x4e, 0x7a, 0x4b, 0xca, 0xae, 0xc4,
            0xdb, 0x48, 0xa2, 0x17, 0xd1, 0x25, 0xd7, 0x0c, 0xd4, 0x4c, 0x08, 0x24, 0x39, 0x32, 0x37, 0xb4, 0x8a, 0x2b,
            0x2a, 0x06, 0xaa, 0x20, 0x2a, 0xfb, 0xbf, 0xb3, 0xf2, 0x01, 0x91, 0xea, 0x57, 0xd6,
        ];
        let want: Vec<u8> = (0..60u32)
            .flat_map(|i| format!("line {}: the quick brown fox jumps over the lazy dog\n", i * i % 97).into_bytes())
            .collect();
        assert_eq!((z[2] >> 1) & 3, 2, "dynamic block");
        assert_eq!(zlib_decompress(&z, 4000), Some(want));
        // And a stored block.
        let stored = [0x78, 0x01, 0x01, 0x03, 0x00, 0xfc, 0xff, b'a', b'b', b'c', 0x02, 0x4d, 0x01, 0x27];
        assert_eq!(zlib_decompress(&stored, 10).as_deref(), Some(&b"abc"[..]));
    }
}
