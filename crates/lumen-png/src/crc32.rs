//! CRC-32 as PNG (and zlib's `crc32`, and Python's `zlib.crc32`) computes
//! it: the reflected polynomial 0xEDB88320, starting from all ones and
//! inverted at the end. A byte at a time from a table built at compile
//! time.

/// The CRC of every byte value, for the reflected polynomial.
const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// A CRC-32 over bytes fed in pieces.
#[derive(Clone, Copy, Debug)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    pub const fn new() -> Self {
        Crc32(!0)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let mut c = self.0;
        for &b in bytes {
            c = TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    /// The CRC of everything fed so far.
    pub const fn finish(self) -> u32 {
        !self.0
    }
}

/// The CRC-32 of `bytes`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(bytes);
    c.finish()
}
