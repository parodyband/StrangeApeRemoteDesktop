//! Byte-blob access to C structs using generated offsets (see `layout.rs`).
//! (Same as `sard-linux`'s `nv/blob.rs`.)

use std::ffi::c_void;

/// Field: (byte offset, size in bytes).
#[derive(Clone, Copy, Debug)]
pub struct F(pub usize, pub usize);

/// Bitfield: (byte offset of the containing u32, shift, width).
#[derive(Clone, Copy, Debug)]
pub struct B(pub usize, pub u32, pub u32);

/// Windows-style GUID, passed by value to NVENC.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid {
    pub d1: u32,
    pub d2: u16,
    pub d3: u16,
    pub d4: [u8; 8],
}

/// Zeroed, 8-byte aligned storage for one C struct.
pub struct Blob {
    buf: Vec<u64>,
    len: usize,
}

impl Blob {
    pub fn new(size: usize) -> Self {
        Self { buf: vec![0; size.div_ceil(8)], len: size }
    }

    pub fn ptr(&mut self) -> *mut c_void {
        self.buf.as_mut_ptr().cast()
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: buf holds at least len initialized bytes.
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast(), self.len) }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, unique borrow.
        unsafe { std::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast(), self.len) }
    }

    pub fn set(&mut self, f: F, v: u64) {
        let b = &mut self.bytes_mut()[f.0..f.0 + f.1];
        match f.1 {
            1 => b[0] = v as u8,
            2 => b.copy_from_slice(&(v as u16).to_ne_bytes()),
            4 => b.copy_from_slice(&(v as u32).to_ne_bytes()),
            8 => b.copy_from_slice(&v.to_ne_bytes()),
            n => panic!("unsupported field size {n}"),
        }
    }

    pub fn get(&self, f: F) -> u64 {
        let b = &self.bytes()[f.0..f.0 + f.1];
        match f.1 {
            1 => b[0] as u64,
            2 => u16::from_ne_bytes(b.try_into().unwrap()) as u64,
            4 => u32::from_ne_bytes(b.try_into().unwrap()) as u64,
            8 => u64::from_ne_bytes(b.try_into().unwrap()),
            n => panic!("unsupported field size {n}"),
        }
    }

    pub fn set_ptr<T>(&mut self, f: F, p: *const T) {
        assert_eq!(f.1, 8);
        self.set(f, p as usize as u64);
    }

    pub fn get_ptr(&self, f: F) -> *mut c_void {
        self.get(f) as usize as *mut c_void
    }

    pub fn set_bits(&mut self, b: B, v: u32) {
        let f = F(b.0, 4);
        let word = self.get(f) as u32;
        let mask = if b.2 >= 32 { u32::MAX } else { ((1u32 << b.2) - 1) << b.1 };
        self.set(f, ((word & !mask) | ((v << b.1) & mask)) as u64);
    }

    pub fn get_bits(&self, b: B) -> u32 {
        let word = self.get(F(b.0, 4)) as u32;
        let mask = if b.2 >= 32 { u32::MAX } else { (1u32 << b.2) - 1 };
        (word >> b.1) & mask
    }

    pub fn set_guid(&mut self, f: F, g: &Guid) {
        assert_eq!(f.1, 16);
        // SAFETY: Guid is repr(C), 16 bytes, no padding.
        let src: [u8; 16] = unsafe { std::mem::transmute(*g) };
        self.bytes_mut()[f.0..f.0 + 16].copy_from_slice(&src);
    }

    /// Copy `len` bytes from `src` into this blob at `off`.
    pub fn copy_in(&mut self, off: usize, src: &[u8]) {
        self.bytes_mut()[off..off + src.len()].copy_from_slice(src);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_and_fields() {
        let mut b = Blob::new(16);
        b.set(F(0, 4), 0xAABBCCDD);
        b.set_bits(B(4, 9, 2), 3);
        b.set_bits(B(4, 0, 1), 1);
        assert_eq!(b.get(F(0, 4)), 0xAABBCCDD);
        assert_eq!(b.get_bits(B(4, 9, 2)), 3);
        assert_eq!(b.get(F(4, 4)), (3 << 9) | 1);
        b.set_bits(B(4, 9, 2), 1);
        assert_eq!(b.get(F(4, 4)), (1 << 9) | 1);
    }
}
