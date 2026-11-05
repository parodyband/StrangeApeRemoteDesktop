//! Systematic Reed-Solomon erasure code over GF(2^8) using a Cauchy matrix.
//!
//! A block has `k` data shards and `m` parity shards (`k + m <= 256`), all of
//! equal size. Parity shard `i` is `sum_j C[i][j] * data[j]` with
//! `C[i][j] = 1 / (x_i ^ y_j)`, `x_i = k + i`, `y_j = j`. Every square
//! submatrix of a Cauchy matrix is invertible, so *any* `k` of the `k + m`
//! shards recover the block. Recovery only inverts the small `n x n` system
//! for the `n` missing data shards, so the common case (one or two lost
//! packets) is cheap.

use std::sync::OnceLock;

const POLY: u16 = 0x11D;

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
    /// Full multiplication table: `mul[a][b] = a*b`. 64 KiB, L2-resident.
    mul: Box<[[u8; 256]; 256]>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for (i, e) in exp.iter_mut().enumerate().take(255) {
            *e = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= POLY;
            }
        }
        exp.copy_within(0..257, 255);
        let mut mul: Box<[[u8; 256]; 256]> = vec![[0u8; 256]; 256].into_boxed_slice().try_into().unwrap();
        for a in 1..256 {
            for b in 1..256 {
                mul[a][b] = exp[log[a] as usize + log[b] as usize];
            }
        }
        Tables { exp, log, mul }
    })
}

#[inline]
pub fn gf_mul(a: u8, b: u8) -> u8 {
    tables().mul[a as usize][b as usize]
}

#[inline]
pub fn gf_inv(a: u8) -> u8 {
    assert!(a != 0, "gf_inv(0)");
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

#[inline]
fn cauchy(k: usize, i: usize, j: usize) -> u8 {
    gf_inv(((k + i) as u8) ^ (j as u8))
}

/// `dst ^= c * src`
#[inline]
fn mul_add(dst: &mut [u8], src: &[u8], c: u8) {
    debug_assert_eq!(dst.len(), src.len());
    match c {
        0 => {}
        1 => {
            // Word-at-a-time XOR.
            let (d8, dr) = dst.as_chunks_mut::<8>();
            let (s8, sr) = src.as_chunks::<8>();
            for (d, s) in d8.iter_mut().zip(s8) {
                *d = (u64::from_ne_bytes(*d) ^ u64::from_ne_bytes(*s)).to_ne_bytes();
            }
            for (d, s) in dr.iter_mut().zip(sr) {
                *d ^= *s;
            }
        }
        _ => {
            let row = &tables().mul[c as usize];
            for (d, s) in dst.iter_mut().zip(src) {
                *d ^= row[*s as usize];
            }
        }
    }
}

pub const MAX_SHARDS: usize = 256;

/// Compute `m` parity shards for `k` data shards. `shards` holds `k + m`
/// shards of `shard_size` bytes back to back; the parity part is overwritten.
pub fn encode(k: usize, m: usize, shard_size: usize, shards: &mut [u8]) {
    assert!(k > 0 && k + m <= MAX_SHARDS);
    assert_eq!(shards.len(), (k + m) * shard_size);
    let (data, parity) = shards.split_at_mut(k * shard_size);
    for i in 0..m {
        let p = &mut parity[i * shard_size..(i + 1) * shard_size];
        p.fill(0);
        for j in 0..k {
            mul_add(p, &data[j * shard_size..(j + 1) * shard_size], cauchy(k, i, j));
        }
    }
}

/// Compute a single parity shard `i` (used for on-demand extra parity).
pub fn encode_one(k: usize, i: usize, shard_size: usize, data: &[u8], out: &mut [u8]) {
    assert!(k + i < MAX_SHARDS);
    out.fill(0);
    for j in 0..k {
        mul_add(out, &data[j * shard_size..(j + 1) * shard_size], cauchy(k, i, j));
    }
}

/// Invert an `n x n` matrix in place (Gauss-Jordan). Returns false if singular.
fn invert(n: usize, a: &mut [u8]) -> bool {
    let mut inv = vec![0u8; n * n];
    for i in 0..n {
        inv[i * n + i] = 1;
    }
    for col in 0..n {
        let Some(pivot) = (col..n).find(|&r| a[r * n + col] != 0) else {
            return false;
        };
        if pivot != col {
            for c in 0..n {
                a.swap(pivot * n + c, col * n + c);
                inv.swap(pivot * n + c, col * n + c);
            }
        }
        let p = gf_inv(a[col * n + col]);
        for c in 0..n {
            a[col * n + c] = gf_mul(a[col * n + c], p);
            inv[col * n + c] = gf_mul(inv[col * n + c], p);
        }
        for r in 0..n {
            if r != col {
                let f = a[r * n + col];
                if f != 0 {
                    for c in 0..n {
                        a[r * n + c] ^= gf_mul(f, a[col * n + c]);
                        inv[r * n + c] ^= gf_mul(f, inv[col * n + c]);
                    }
                }
            }
        }
    }
    a.copy_from_slice(&inv);
    true
}

/// Recover missing data shards in place. `shards` is the `(k + m) * shard_size`
/// block buffer, `present[i]` says whether shard `i` arrived. Missing data
/// shards are rewritten and marked present. Returns false if fewer than `k`
/// shards are available.
pub fn reconstruct(k: usize, m: usize, shard_size: usize, shards: &mut [u8], present: &mut [bool]) -> bool {
    assert_eq!(shards.len(), (k + m) * shard_size);
    assert_eq!(present.len(), k + m);
    let missing: Vec<usize> = (0..k).filter(|&j| !present[j]).collect();
    if missing.is_empty() {
        return true;
    }
    let parity: Vec<usize> = (0..m).filter(|&i| present[k + i]).take(missing.len()).collect();
    if parity.len() < missing.len() {
        return false;
    }
    let n = missing.len();

    // RHS: for each used parity row, subtract (xor) contributions of known data.
    let mut rhs = vec![0u8; n * shard_size];
    {
        let (data, par) = shards.split_at(k * shard_size);
        for (r, &pi) in parity.iter().enumerate() {
            let out = &mut rhs[r * shard_size..(r + 1) * shard_size];
            out.copy_from_slice(&par[pi * shard_size..(pi + 1) * shard_size]);
            for j in 0..k {
                if present[j] {
                    mul_add(out, &data[j * shard_size..(j + 1) * shard_size], cauchy(k, pi, j));
                }
            }
        }
    }

    // Square Cauchy submatrix restricted to the missing columns.
    let mut a = vec![0u8; n * n];
    for (r, &pi) in parity.iter().enumerate() {
        for (c, &mj) in missing.iter().enumerate() {
            a[r * n + c] = cauchy(k, pi, mj);
        }
    }
    if !invert(n, &mut a) {
        return false; // cannot happen for a Cauchy matrix
    }

    for (c, &mj) in missing.iter().enumerate() {
        let out = &mut shards[mj * shard_size..(mj + 1) * shard_size];
        out.fill(0);
        for r in 0..n {
            mul_add(out, &rhs[r * shard_size..(r + 1) * shard_size], a[c * n + r]);
        }
        present[mj] = true;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rand::XorShift;

    #[test]
    fn field_basics() {
        for a in 1..=255u8 {
            assert_eq!(gf_mul(a, gf_inv(a)), 1);
            assert_eq!(gf_mul(a, 1), a);
            assert_eq!(gf_mul(a, 0), 0);
        }
        // distributivity spot check
        assert_eq!(gf_mul(7, 3 ^ 9), gf_mul(7, 3) ^ gf_mul(7, 9));
    }

    fn roundtrip(k: usize, m: usize, size: usize, rng: &mut XorShift) {
        let mut buf = vec![0u8; (k + m) * size];
        for b in buf[..k * size].iter_mut() {
            *b = rng.next_u64() as u8;
        }
        encode(k, m, size, &mut buf);
        let original = buf.clone();
        // Erase up to m random shards (data or parity).
        let mut present = vec![true; k + m];
        let erase = rng.below(m as u64 + 1) as usize;
        let mut erased = 0;
        while erased < erase {
            let i = rng.below((k + m) as u64) as usize;
            if present[i] {
                present[i] = false;
                buf[i * size..(i + 1) * size].fill(0xEE);
                erased += 1;
            }
        }
        assert!(reconstruct(k, m, size, &mut buf, &mut present), "k={k} m={m} erased={erase}");
        assert_eq!(&buf[..k * size], &original[..k * size]);
    }

    #[test]
    fn random_erasures() {
        let mut rng = XorShift(0x1234_5678_9abc_def1);
        for _ in 0..300 {
            let k = 1 + rng.below(80) as usize;
            let m = rng.below(20) as usize + 1;
            let size = 1 + rng.below(300) as usize;
            roundtrip(k, m, size, &mut rng);
        }
        roundtrip(200, 56, 64, &mut rng);
    }

    #[test]
    fn too_many_erasures_fails() {
        let (k, m, s) = (10, 2, 16);
        let mut buf = vec![1u8; (k + m) * s];
        encode(k, m, s, &mut buf);
        let mut present = vec![true; k + m];
        present[0] = false;
        present[1] = false;
        present[2] = false;
        assert!(!reconstruct(k, m, s, &mut buf, &mut present));
    }

    #[test]
    fn encode_one_matches() {
        let (k, m, s) = (5, 3, 32);
        let mut buf: Vec<u8> = (0..(k + m) * s).map(|i| i as u8).collect();
        encode(k, m, s, &mut buf);
        let mut out = vec![0u8; s];
        encode_one(k, 2, s, &buf[..k * s], &mut out);
        assert_eq!(&out[..], &buf[(k + 2) * s..(k + 3) * s]);
    }
}
