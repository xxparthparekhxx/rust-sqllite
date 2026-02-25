/// A variable-length integer (varint) encoding defined by SQLite.
///
/// A varint is between 1 and 9 bytes in length. The varint consists of either zero or more bytes which have the high-order bit set followed by a single byte with the high-order bit clear, or nine bytes, whichever is shorter.
/// The lower seven bits of each of the first eight bytes and all 8 bits of the ninth byte are used to reconstruct the 64-bit unsigned integer.

pub fn get_varint(buf: &[u8]) -> (u64, usize) {
    let mut v: u64 = 0;
    for i in 0..8 {
        if i >= buf.len() {
            return (v, i);
        }
        let byte = buf[i];
        v = (v << 7) | (byte & 0x7F) as u64;
        if (byte & 0x80) == 0 {
            return (v, i + 1);
        }
    }
    if buf.len() > 8 {
        v = (v << 8) | buf[8] as u64;
        (v, 9)
    } else {
        (v, 8)
    }
}

pub fn put_varint(buf: &mut [u8], mut v: u64) -> usize {
    if v <= 0x7f {
        buf[0] = (v & 0x7f) as u8;
        return 1;
    }
    if v <= 0x3fff {
        buf[0] = (((v >> 7) & 0x7f) | 0x80) as u8;
        buf[1] = (v & 0x7f) as u8;
        return 2;
    }

    // Convert to byte array
    let mut tmp = [0u8; 9];
    let _n = 0;

    // Handle the 9th byte which takes 8 bits
    if v > 0x00FFFFFFFFFFFFFF {
        tmp[8] = (v & 0xFF) as u8;
        v >>= 8;
        for i in (0..8).rev() {
            tmp[i] = ((v & 0x7F) | 0x80) as u8;
            v >>= 7;
        }
        buf[..9].copy_from_slice(&tmp);
        return 9;
    }

    // Normal 1 to 8 byte varints (each taking 7 bits)
    let mut tmp2 = [0u8; 9];
    let mut i = 8;
    tmp2[i] = (v & 0x7F) as u8;
    v >>= 7;
    while v > 0 {
        i -= 1;
        tmp2[i] = ((v & 0x7F) | 0x80) as u8;
        v >>= 7;
    }
    let len = 9 - i;
    buf[..len].copy_from_slice(&tmp2[i..]);
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_roundtrip() {
        let values = [
            0,
            1,
            127,
            128,
            255,
            16383,
            16384,
            2097151,
            2097152,
            0x00FFFFFFFFFFFFFF,
            0x01FFFFFFFFFFFFFF,
            u64::MAX,
        ];

        let mut buf = [0u8; 9];
        for &v in &values {
            let encoded_len = put_varint(&mut buf, v);
            let (decoded_v, decoded_len) = get_varint(&buf[..encoded_len]);
            assert_eq!(decoded_v, v, "Failed for value {}", v);
            assert_eq!(decoded_len, encoded_len, "Length mismatch for value {}", v);
        }
    }
}
