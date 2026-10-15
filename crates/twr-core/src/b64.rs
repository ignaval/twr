//! Standard base64 (RFC 4648, `+/`, `=` padding) for binary `blob.get` chunks. Tiny on purpose:
//! no dependency for one call site.

use anyhow::bail;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `bytes` with padding.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn value(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    } as u32)
}

/// Decodes padded base64. Rejects bad characters, bad lengths and misplaced padding.
pub fn decode(s: &str) -> anyhow::Result<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        bail!("base64 length {} is not a multiple of 4", s.len());
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let quads = s.len() / 4;
    for (qi, q) in s.chunks(4).enumerate() {
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && qi + 1 != quads) {
            bail!("misplaced base64 padding");
        }
        let mut n = 0u32;
        for &c in &q[..4 - pad] {
            let Some(v) = value(c) else {
                bail!("invalid base64 character {:?}", c as char);
            };
            n = (n << 6) | v;
        }
        n <<= 6 * pad as u32;
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), enc);
            assert_eq!(decode(enc).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn round_trips_all_bytes_and_rejects_garbage() {
        let all: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for n in 0..all.len().min(20) {
            assert_eq!(decode(&encode(&all[..n])).unwrap(), &all[..n]);
        }
        assert_eq!(decode(&encode(&all)).unwrap(), all);
        assert!(decode("Zm9").is_err());
        assert!(decode("Zm9$").is_err());
        assert!(decode("Zg==Zm9v").is_err());
        assert!(decode("Z===").is_err());
    }
}
