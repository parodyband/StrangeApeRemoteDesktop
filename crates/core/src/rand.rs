//! OS-backed cryptographically secure random bytes.

/// Fill `buf` with random bytes from the OS CSPRNG. Panics if the OS refuses,
/// which only happens on a badly broken system.
pub fn fill(buf: &mut [u8]) {
    #[cfg(target_os = "linux")]
    {
        let mut off = 0;
        while off < buf.len() {
            // SAFETY: pointer/len describe the unfilled tail of `buf`.
            let r = unsafe { libc::getrandom(buf[off..].as_mut_ptr().cast(), buf.len() - off, 0) };
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("getrandom failed: {e}");
            }
            off += r as usize;
        }
    }
    #[cfg(target_os = "macos")]
    {
        for chunk in buf.chunks_mut(256) {
            // SAFETY: chunk is at most 256 bytes as getentropy requires.
            let r = unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) };
            assert_eq!(r, 0, "getentropy failed: {}", std::io::Error::last_os_error());
        }
    }
    #[cfg(windows)]
    {
        const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;
        #[link(name = "bcrypt")]
        unsafe extern "system" {
            fn BCryptGenRandom(alg: *mut std::ffi::c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
        }
        for chunk in buf.chunks_mut(1 << 20) {
            // SAFETY: chunk is a valid writable buffer of the given length.
            let r = unsafe {
                BCryptGenRandom(
                    std::ptr::null_mut(),
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                    BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                )
            };
            assert_eq!(r, 0, "BCryptGenRandom failed: NTSTATUS {r:#x}");
        }
    }
}

pub fn bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    fill(&mut b);
    b
}

pub fn u32() -> u32 {
    u32::from_le_bytes(bytes())
}

pub fn u64() -> u64 {
    u64::from_le_bytes(bytes())
}

/// Small fast non-crypto PRNG (xorshift64*) for tests and simulations.
pub struct XorShift(pub u64);

impl XorShift {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
    pub fn chance(&mut self, p: f64) -> bool {
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}
