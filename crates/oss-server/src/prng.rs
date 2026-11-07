//! A small, fast pseudo-random generator for game decisions (which radar
//! positions to send, random prizes). Not for anything security-related.

/// xorshift64* generator.
#[derive(Debug, Clone)]
pub struct Prng(u64);

impl Default for Prng {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64);
        Self::new(seed)
    }
}

impl Prng {
    /// A generator with a fixed seed (zero is replaced).
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// Next 32 random bits.
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    /// A value in `0..n` (0 when `n` is 0).
    pub fn below(&mut self, n: u32) -> u32 {
        if n == 0 {
            0
        } else {
            ((u64::from(self.next_u32()) * u64::from(n)) >> 32) as u32
        }
    }

    /// True with probability `num / den`.
    pub fn chance(&mut self, num: u32, den: u32) -> bool {
        self.below(den) < num
    }
}
