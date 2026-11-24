//! The two base32 alphabets the protocol uses (docs/relay-protocol.md §2, §9): RFC 4648 in
//! lowercase without padding, for names and tags, and z-base-32, for pkarr keys.

const RFC4648: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const ZBASE32: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";

fn encode_with(alphabet: &[u8; 32], bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(alphabet[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(alphabet[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn decode_with(alphabet: &[u8; 32], text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in text.bytes() {
        let v = alphabet.iter().position(|&a| a == c)? as u32;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Leftover bits must be zero padding, fewer than 5 of them: one text, one value.
    if bits >= 5 || acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

/// RFC 4648 base32, lowercase, no padding.
pub fn encode(bytes: &[u8]) -> String {
    encode_with(RFC4648, bytes)
}

/// The inverse of [`encode`]. Uppercase, padding and other characters are refused.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    decode_with(RFC4648, text)
}

/// Whether `text` is `len` characters of the lowercase RFC 4648 alphabet.
pub fn is_label(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|c| RFC4648.contains(&c))
}

/// z-base-32, as pkarr writes keys.
pub fn zencode(bytes: &[u8]) -> String {
    encode_with(ZBASE32, bytes)
}

/// The inverse of [`zencode`].
pub fn zdecode(text: &str) -> Option<Vec<u8>> {
    decode_with(ZBASE32, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        // RFC 4648 §10, lowercased, padding removed.
        for (plain, enc) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(encode(plain.as_bytes()), enc);
            assert_eq!(decode(enc).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn refuses_other_forms() {
        assert!(decode("MZXW6").is_none());
        assert!(decode("mzxw6===").is_none());
        assert!(decode("mzxw1").is_none());
        // Non-zero trailing bits: "my" is "f"; "mz" has a stray bit.
        assert!(decode("mz").is_none());
        // A dangling character that can't hold a byte.
        assert!(decode("m").is_none());
    }

    #[test]
    fn zbase32_roundtrip_matches_pkarr_length() {
        let key = [7u8; 32];
        let z = zencode(&key);
        assert_eq!(z.len(), 52);
        assert_eq!(zdecode(&z).unwrap(), key);
    }

    #[test]
    fn roundtrip_all_lengths() {
        for n in 0..40u8 {
            let bytes: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37).wrapping_add(n)).collect();
            assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
            assert_eq!(zdecode(&zencode(&bytes)).unwrap(), bytes);
        }
    }
}
