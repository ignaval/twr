//! Reading the server name and ALPN list from a TLS ClientHello without decrypting or
//! consuming anything (docs/relay-protocol.md §4). Input is untrusted: every length is
//! checked, and nothing here allocates more than the input's size.

/// The largest ClientHello the relay waits for (§4, rule 1).
pub const MAX_HELLO: usize = 16 * 1024;
/// Enough bytes to hold a maximal ClientHello split over records.
pub const PEEK_BUFFER: usize = MAX_HELLO + 4 + 5 * 4;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hello {
    /// The host name, lowercased, without a trailing dot.
    pub sni: Option<String>,
    pub alpn: Vec<Vec<u8>>,
}

impl Hello {
    pub fn offers_alpn(&self, proto: &[u8]) -> bool {
        self.alpn.iter().any(|p| p == proto)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parse {
    /// More bytes are needed.
    Incomplete,
    /// Not a TLS ClientHello, or a malformed one.
    Invalid,
    /// Over [`MAX_HELLO`].
    TooLarge,
    Done(Hello),
}

/// Collects the handshake bytes from the records at the start of `buf`, then parses the
/// ClientHello once its whole length has arrived.
pub fn parse(buf: &[u8]) -> Parse {
    let mut handshake: Vec<u8> = Vec::new();
    let mut rest = buf;
    loop {
        if rest.len() < 5 {
            return Parse::Incomplete;
        }
        // ContentType handshake, legacy record version 3.x.
        if rest[0] != 22 || rest[1] != 3 || rest[2] > 4 {
            return Parse::Invalid;
        }
        let len = u16::from_be_bytes([rest[3], rest[4]]) as usize;
        if len == 0 || len > 16384 {
            return Parse::Invalid;
        }
        if rest.len() < 5 + len {
            // Check what we have so far: a wrong message type fails early.
            handshake.extend_from_slice(&rest[5..]);
            return early(&handshake);
        }
        handshake.extend_from_slice(&rest[5..5 + len]);
        rest = &rest[5 + len..];
        if handshake.len() >= 4 {
            if handshake[0] != 1 {
                return Parse::Invalid;
            }
            let body_len = u32::from_be_bytes([0, handshake[1], handshake[2], handshake[3]]) as usize;
            if body_len > MAX_HELLO {
                return Parse::TooLarge;
            }
            if handshake.len() >= 4 + body_len {
                return match client_hello(&handshake[4..4 + body_len]) {
                    Some(h) => Parse::Done(h),
                    None => Parse::Invalid,
                };
            }
        }
        if handshake.len() > MAX_HELLO + 4 {
            return Parse::TooLarge;
        }
    }
}

fn early(handshake: &[u8]) -> Parse {
    if let Some(&t) = handshake.first() {
        if t != 1 {
            return Parse::Invalid;
        }
    }
    if handshake.len() >= 4 {
        let body_len = u32::from_be_bytes([0, handshake[1], handshake[2], handshake[3]]) as usize;
        if body_len > MAX_HELLO {
            return Parse::TooLarge;
        }
    }
    Parse::Incomplete
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn vec8(&mut self) -> Option<&'a [u8]> {
        let n = self.u8()? as usize;
        self.take(n)
    }
    fn vec16(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()? as usize;
        self.take(n)
    }
}

fn client_hello(body: &[u8]) -> Option<Hello> {
    let mut r = Reader(body);
    let _legacy_version = r.u16()?;
    r.take(32)?; // random
    let session = r.vec8()?;
    if session.len() > 32 {
        return None;
    }
    let suites = r.vec16()?;
    if suites.is_empty() || suites.len() % 2 != 0 {
        return None;
    }
    let compression = r.vec8()?;
    if compression.is_empty() {
        return None;
    }
    let mut hello = Hello::default();
    if r.0.is_empty() {
        return Some(hello); // no extensions
    }
    let mut exts = Reader(r.vec16()?);
    if !r.0.is_empty() {
        return None;
    }
    let mut seen_sni = false;
    let mut seen_alpn = false;
    while !exts.0.is_empty() {
        let kind = exts.u16()?;
        let data = exts.vec16()?;
        match kind {
            0 => {
                if seen_sni {
                    return None;
                }
                seen_sni = true;
                hello.sni = server_name(data)?;
            }
            16 => {
                if seen_alpn {
                    return None;
                }
                seen_alpn = true;
                let mut list = Reader(data);
                let mut names = Reader(list.vec16()?);
                if !list.0.is_empty() {
                    return None;
                }
                while !names.0.is_empty() {
                    let p = names.vec8()?;
                    if p.is_empty() {
                        return None;
                    }
                    hello.alpn.push(p.to_vec());
                }
                if hello.alpn.is_empty() {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some(hello)
}

/// The host_name entry of a server_name extension. `Some(None)` for an empty extension, which
/// a server sends back but a client never should; treat it as no name.
fn server_name(data: &[u8]) -> Option<Option<String>> {
    if data.is_empty() {
        return Some(None);
    }
    let mut r = Reader(data);
    let mut list = Reader(r.vec16()?);
    if !r.0.is_empty() {
        return None;
    }
    let mut name = None;
    while !list.0.is_empty() {
        let kind = list.u8()?;
        let value = list.vec16()?;
        if kind == 0 {
            if name.is_some() {
                return None;
            }
            name = Some(normalize(value)?);
        }
    }
    Some(name)
}

fn normalize(raw: &[u8]) -> Option<String> {
    let raw = raw.strip_suffix(b".").unwrap_or(raw);
    if raw.is_empty() || raw.len() > 253 {
        return None;
    }
    if !raw.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.' || c == b'_') {
        return None;
    }
    Some(String::from_utf8(raw.to_ascii_lowercase()).ok()?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A ClientHello made by rustls itself, as the phone or hub would send it.
    pub fn rustls_hello(sni: Option<&str>, alpn: &[&[u8]]) -> Vec<u8> {
        use rustls::pki_types::ServerName;
        let mut cfg = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(crate::tls::PinVerifier::new([0; 32])))
        .with_no_client_auth();
        cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
        let name = match sni {
            Some(s) => ServerName::try_from(s.to_string()).unwrap(),
            None => ServerName::IpAddress("192.0.2.1".parse::<std::net::IpAddr>().unwrap().into()),
        };
        let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(cfg), name).unwrap();
        let mut out = Vec::new();
        conn.write_tls(&mut out).unwrap();
        out
    }

    #[test]
    fn reads_rustls_hellos() {
        let h = rustls_hello(Some("Hub-Tag.EU.relay.twr.tools"), &[b"twr-relay/1", b"h2"]);
        let Parse::Done(hello) = parse(&h) else { panic!("{:?}", parse(&h)) };
        assert_eq!(hello.sni.as_deref(), Some("hub-tag.eu.relay.twr.tools"));
        assert_eq!(hello.alpn, vec![b"twr-relay/1".to_vec(), b"h2".to_vec()]);
        let ip = rustls_hello(None, &[]);
        assert_eq!(parse(&ip), Parse::Done(Hello::default()));
    }

    #[test]
    fn every_prefix_is_incomplete() {
        let h = rustls_hello(Some("a.example"), &[b"http/1.1"]);
        for n in 0..h.len() {
            assert_eq!(parse(&h[..n]), Parse::Incomplete, "prefix {n}");
        }
    }

    #[test]
    fn split_over_records() {
        let h = rustls_hello(Some("split.example"), &[b"h2"]);
        let body = &h[5..];
        let (a, b) = body.split_at(40);
        let mut split = vec![22, 3, 1];
        split.extend_from_slice(&(a.len() as u16).to_be_bytes());
        split.extend_from_slice(a);
        split.extend_from_slice(&[22, 3, 1]);
        split.extend_from_slice(&(b.len() as u16).to_be_bytes());
        split.extend_from_slice(b);
        let Parse::Done(hello) = parse(&split) else { panic!() };
        assert_eq!(hello.sni.as_deref(), Some("split.example"));
    }

    #[test]
    fn not_tls() {
        assert_eq!(parse(b"GET / HTTP/1.1\r\n\r\n"), Parse::Invalid);
        assert_eq!(parse(&[22, 3, 1, 0, 0]), Parse::Invalid);
        // A ServerHello (type 2).
        assert_eq!(parse(&[22, 3, 3, 0, 4, 2, 0, 0, 0]), Parse::Invalid);
        // A ClientHello that claims to be huge.
        assert_eq!(parse(&[22, 3, 1, 0, 4, 1, 0xff, 0xff, 0xff]), Parse::TooLarge);
    }
}
