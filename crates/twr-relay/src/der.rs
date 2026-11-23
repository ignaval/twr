//! Just enough DER to find a certificate's SubjectPublicKeyInfo, for pin checks.

/// One DER element: its tag, and the whole element and its contents as slices.
struct Tlv<'a> {
    tag: u8,
    whole: &'a [u8],
    body: &'a [u8],
}

fn read_tlv(input: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    let tag = *input.first()?;
    if tag & 0x1f == 0x1f {
        return None; // multi-byte tags never occur in the fields we walk
    }
    let first = *input.get(1)?;
    let (len, header) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *input.get(2 + i)? as usize;
        }
        if len < 0x80 {
            return None; // not minimal
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    if end > input.len() {
        return None;
    }
    Some((Tlv { tag, whole: &input[..end], body: &input[header..end] }, &input[end..]))
}

/// The DER SubjectPublicKeyInfo of an X.509 certificate, or `None` if it doesn't parse.
pub fn cert_spki(cert: &[u8]) -> Option<&[u8]> {
    let (certificate, rest) = read_tlv(cert)?;
    if certificate.tag != 0x30 || !rest.is_empty() {
        return None;
    }
    let (tbs, _) = read_tlv(certificate.body)?;
    if tbs.tag != 0x30 {
        return None;
    }
    let mut fields = tbs.body;
    // [0] EXPLICIT version, optional.
    let (first, after) = read_tlv(fields)?;
    if first.tag == 0xa0 {
        fields = after;
    }
    // serialNumber, signature, issuer, validity, subject.
    for want in [0x02u8, 0x30, 0x30, 0x30, 0x30] {
        let (field, after) = read_tlv(fields)?;
        if field.tag != want {
            return None;
        }
        fields = after;
    }
    let (spki, _) = read_tlv(fields)?;
    (spki.tag == 0x30).then_some(spki.whole)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_is_refused() {
        assert!(cert_spki(&[]).is_none());
        assert!(cert_spki(&[0x30]).is_none());
        assert!(cert_spki(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]).is_none());
        assert!(cert_spki(&[0x30, 0x81, 0x05, 0, 0, 0, 0, 0]).is_none());
        assert!(cert_spki(&[0x30, 0x00, 0x00]).is_none());
    }
}
