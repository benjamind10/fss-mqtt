//! CRC-32 (IEEE 802.3, as in gzip, zip and PNG), table-driven.

const TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Continues a CRC over more bytes; start with `update(0, …)`.
pub fn update(crc: u32, b: &[u8]) -> u32 {
    let mut c = !crc;
    for &x in b {
        c = TABLE[((c ^ x as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

pub fn crc32(b: &[u8]) -> u32 {
    update(0, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(update(crc32(b"1234"), b"56789"), 0xcbf4_3926);
    }
}
