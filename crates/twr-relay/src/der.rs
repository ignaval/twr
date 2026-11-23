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
    let mut fields = tbs_fields(cert)?;
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

/// The tbsCertificate fields after the optional version: serial, signature, issuer, validity…
fn tbs_fields(cert: &[u8]) -> Option<&[u8]> {
    let (certificate, rest) = read_tlv(cert)?;
    if certificate.tag != 0x30 || !rest.is_empty() {
        return None;
    }
    let (tbs, _) = read_tlv(certificate.body)?;
    if tbs.tag != 0x30 {
        return None;
    }
    let (first, after) = read_tlv(tbs.body)?;
    Some(if first.tag == 0xa0 { after } else { tbs.body })
}

/// A certificate's notAfter as Unix seconds.
pub fn cert_not_after(cert: &[u8]) -> Option<i64> {
    let mut fields = tbs_fields(cert)?;
    for want in [0x02u8, 0x30, 0x30] {
        let (field, after) = read_tlv(fields)?;
        if field.tag != want {
            return None;
        }
        fields = after;
    }
    let (validity, _) = read_tlv(fields)?;
    if validity.tag != 0x30 {
        return None;
    }
    let (_not_before, rest) = read_tlv(validity.body)?;
    let (not_after, _) = read_tlv(rest)?;
    let text = std::str::from_utf8(not_after.body).ok()?;
    let digits = text.strip_suffix('Z')?;
    if !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (year, rest) = match (not_after.tag, digits.len()) {
        (0x17, 12) => {
            let yy: i64 = digits[..2].parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &digits[2..])
        }
        (0x18, 14) => (digits[..4].parse().ok()?, &digits[4..]),
        _ => return None,
    };
    let n = |i: usize| -> i64 { rest[i..i + 2].parse().unwrap_or(-1) };
    let (mo, d, h, mi, s) = (n(0), n(2), n(4), n(6), n(8));
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..61).contains(&s) {
        return None;
    }
    Some(days_from_civil(year, mo, d) * 86400 + h * 3600 + mi * 60 + s)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    #[test]
    fn civil_days() {
        assert_eq!(super::days_from_civil(1970, 1, 1), 0);
        assert_eq!(super::days_from_civil(2000, 3, 1), 11017);
        assert_eq!(super::days_from_civil(2026, 11, 23), 20780);
    }

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
