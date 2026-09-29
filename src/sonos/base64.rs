//! Standard base64 (`+/`, `=` padding), both ways, hand-rolled.
//!
//! Two small needs, not worth a crate: the Sonos sign-in sends its integration
//! credentials as an HTTP Basic header (`login.rs`), and the account envelope a
//! household stores arrives base64-encoded (`stored.rs`). They were written
//! separately and then carried a test-only copy of each other; this is the one
//! copy.

use anyhow::{Result, anyhow};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode, tolerating whitespace - the envelope arrives wrapped - and treating
/// `=` as the padding it is rather than checking where it falls.
pub fn decode(input: &str) -> Result<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for b in input.bytes() {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        let v = val(b).ok_or_else(|| anyhow!("invalid base64 byte {b:#x}"))?;
        bits = (bits << 6) | u32::from(v);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648's own vectors, and the Basic credential shape `login` sends.
    #[test]
    fn encoding_matches_the_standard_vectors() {
        for (plain, encoded) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (b"key:secret", "a2V5OnNlY3JldA=="),
        ] {
            assert_eq!(encode(plain), encoded);
            assert_eq!(decode(encoded).unwrap(), plain);
        }
    }

    #[test]
    fn every_byte_value_survives_the_round_trip() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
    }

    #[test]
    fn decoding_skips_whitespace_and_refuses_what_is_not_base64() {
        assert_eq!(decode("Zm9v\nYmFy \r\n").unwrap(), b"foobar");
        let refused = decode("Zm9v*mFy").unwrap_err();
        assert!(format!("{refused:#}").contains("0x2a"), "{refused:#}");
    }
}
