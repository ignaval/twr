//! Certificate-free phone pairing: codes, the hub's pin, the typed-code pin proof and the
//! version 2 invitation. `docs/design/phone-pairing.md` is the contract; the test vectors in
//! `clients/shared/fixtures/phone-pairing-v2.json` are checked here and by every client.

use anyhow::{bail, ensure, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use ring::{
    hkdf, hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The 32 symbols of a code: no `I`, `O`, `0` or `1`.
pub const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// 12 symbols, 60 bits.
pub const CODE_LEN: usize = 12;
const SALT: &[u8] = b"twr-mobile-pair-v2";
const PROOF_LABEL: &[u8] = b"twr-mobile-pin-v2\0";
/// Bytes of a pin-proof nonce.
pub const NONCE_LEN: usize = 32;
/// The most routes an invitation carries.
pub const MAX_ROUTES: usize = 8;
/// The longest invitation JSON a client accepts.
pub const MAX_INVITATION_BYTES: usize = 2048;
/// The link prefix; the invitation JSON follows as base64url, in the fragment.
pub const LINK_PREFIX: &str = "twr://pair#";

/// A fresh code, normalized (no separators).
pub fn new_code() -> String {
    let mut bytes = [0u8; CODE_LEN];
    SystemRandom::new().fill(&mut bytes).expect("OS entropy");
    bytes
        .into_iter()
        .map(|b| CODE_ALPHABET[(b & 31) as usize] as char)
        .collect()
}

/// `input` with ASCII spaces and `-` removed and letters uppercased, if what is left is
/// exactly a code; `None` otherwise.
pub fn normalize_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| *c != ' ' && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

/// `XXXX-XXXX-XXXX`, for display only.
pub fn display_code(code: &str) -> String {
    match normalize_code(code) {
        Some(c) => format!("{}-{}-{}", &c[..4], &c[4..8], &c[8..]),
        None => code.to_owned(),
    }
}

/// The Argon2id step of [`proof_key`] (64 MiB, 3 passes, one lane), for test vectors.
fn stretch(code: &str) -> Result<[u8; 32]> {
    let code = normalize_code(code).context("not a pairing code")?;
    let params = Params::new(64 * 1024, 3, 1, Some(32))
        .map_err(|_| anyhow::anyhow!("invalid Argon2 parameters"))?;
    let mut stretched = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(code.as_bytes(), SALT, &mut stretched)
        .map_err(|_| anyhow::anyhow!("pairing KDF failed"))?;
    Ok(stretched)
}

/// `K_proof`: Argon2id (64 MiB, 3 passes) of the normalized code, then HKDF-SHA256.
pub fn proof_key(code: &str) -> Result<[u8; 32]> {
    let stretched = stretch(code)?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, SALT).extract(&stretched);
    let info = [b"pin-proof".as_slice()];
    let okm = prk
        .expand(&info, hkdf::HKDF_SHA256)
        .map_err(|_| anyhow::anyhow!("pairing HKDF failed"))?;
    let mut key = [0u8; 32];
    okm.fill(&mut key)
        .map_err(|_| anyhow::anyhow!("pairing HKDF failed"))?;
    Ok(key)
}

/// The SHA-256 of a SubjectPublicKeyInfo DER: the raw pin.
pub fn pin_bytes(spki_der: &[u8]) -> [u8; 32] {
    Sha256::digest(spki_der).into()
}

/// The pin as invitations and `/health` carry it: base64url without padding, 43 characters.
pub fn pin(spki_der: &[u8]) -> String {
    b64url_encode(&pin_bytes(spki_der))
}

/// Decodes a pin string to its 32 bytes.
pub fn decode_pin(pin: &str) -> Result<[u8; 32]> {
    let bytes = b64url_decode(pin)?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("a pin is 32 bytes"))
}

/// The pin proof for `pin` (raw), `nonce` (32 bytes) and `gateway_id`.
pub fn pin_proof(
    key: &[u8; 32],
    pin: &[u8; 32],
    nonce: &[u8],
    gateway_id: &str,
) -> Result<[u8; 32]> {
    ensure!(nonce.len() == NONCE_LEN, "a nonce is {NONCE_LEN} bytes");
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut message = Vec::with_capacity(PROOF_LABEL.len() + 64 + gateway_id.len());
    message.extend_from_slice(PROOF_LABEL);
    message.extend_from_slice(pin);
    message.extend_from_slice(nonce);
    message.extend_from_slice(gateway_id.as_bytes());
    let tag = hmac::sign(&key, &message);
    Ok(tag.as_ref().try_into().expect("HMAC-SHA256 is 32 bytes"))
}

/// Checks `proof` against what `key` gives, in constant time.
pub fn verify_pin_proof(
    key: &[u8; 32],
    pin: &[u8; 32],
    nonce: &[u8],
    gateway_id: &str,
    proof: &[u8],
) -> bool {
    use subtle::ConstantTimeEq;
    match pin_proof(key, pin, nonce, gateway_id) {
        Ok(expected) => bool::from(expected.as_slice().ct_eq(proof)),
        Err(_) => false,
    }
}

/// One entry of an invitation's route list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// Direct TLS to the gateway at `host` (an IPv4 literal, an IPv6 literal without brackets,
    /// or a DNS name) and `port`, authenticated by the pin.
    Tls { host: String, port: u16 },
    /// A route kind this version doesn't know (`relay:` in phase 1): kept, never dialed.
    Other(String),
}

impl Route {
    pub fn parse(text: &str) -> Result<Self> {
        ensure!(!text.is_empty() && text.len() <= 255, "bad route");
        let (kind, rest) = text.split_once(':').context("a route is <kind>:<rest>")?;
        ensure!(
            !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_lowercase()),
            "bad route kind"
        );
        if kind != "tls" {
            return Ok(Route::Other(text.to_owned()));
        }
        let (host, port) = rest.rsplit_once(':').context("tls route needs a port")?;
        let port: u16 = port.parse().context("bad port")?;
        ensure!(port != 0, "bad port");
        let host = if let Some(inner) = host.strip_prefix('[') {
            let inner = inner.strip_suffix(']').context("bad IPv6 literal")?;
            inner
                .parse::<std::net::Ipv6Addr>()
                .context("bad IPv6 literal")?;
            inner.to_owned()
        } else if host.parse::<std::net::Ipv4Addr>().is_ok() {
            host.to_owned()
        } else {
            ensure!(valid_dns_name(host), "bad host");
            host.to_ascii_lowercase()
        };
        Ok(Route::Tls { host, port })
    }
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Route::Tls { host, port } if host.contains(':') => write!(f, "tls:[{host}]:{port}"),
            Route::Tls { host, port } => write!(f, "tls:{host}:{port}"),
            Route::Other(text) => f.write_str(text),
        }
    }
}

fn valid_dns_name(name: &str) -> bool {
    name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && name.bytes().any(|b| b.is_ascii_alphabetic())
}

/// A version 2 invitation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Invitation {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub name: String,
    pub pin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iroh: Option<String>,
    pub code: String,
    pub expires_at: u64,
    pub routes: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl Invitation {
    /// Every field an invitation carries; the hub has each at hand when it opens one.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gateway_id: &str,
        name: &str,
        pin: &str,
        iroh: Option<String>,
        code: &str,
        expires_at_ms: u64,
        routes: &[Route],
        scopes: &[String],
    ) -> Result<Self> {
        let invitation = Invitation {
            kind: "twr-pair".into(),
            version: 2,
            id: gateway_id.into(),
            name: name.chars().take(80).collect(),
            pin: pin.into(),
            iroh,
            code: normalize_code(code).context("not a pairing code")?,
            expires_at: expires_at_ms,
            routes: routes
                .iter()
                .take(MAX_ROUTES)
                .map(Route::to_string)
                .collect(),
            scopes: scopes.to_vec(),
        };
        invitation.validate()?;
        Ok(invitation)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == "twr-pair" && self.version == 2,
            "not a version 2 invitation"
        );
        uuid::Uuid::parse_str(&self.id).context("bad gateway id")?;
        ensure!(self.name.chars().count() <= 80, "name too long");
        decode_pin(&self.pin)?;
        ensure!(
            normalize_code(&self.code).as_deref() == Some(self.code.as_str()),
            "bad code"
        );
        ensure!(
            (1..=MAX_ROUTES).contains(&self.routes.len()),
            "an invitation has 1 to {MAX_ROUTES} routes"
        );
        for route in &self.routes {
            Route::parse(route)?;
        }
        ensure!(
            self.to_json().len() <= MAX_INVITATION_BYTES,
            "invitation too large"
        );
        Ok(())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("invitation serializes")
    }

    /// `twr://pair#<base64url(JSON)>`.
    pub fn to_link(&self) -> String {
        format!("{LINK_PREFIX}{}", b64url_encode(self.to_json().as_bytes()))
    }

    /// Accepts the JSON or the link form.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let json = match text.strip_prefix(LINK_PREFIX) {
            Some(encoded) => String::from_utf8(b64url_decode(encoded)?).context("bad link")?,
            None => text.to_owned(),
        };
        ensure!(json.len() <= MAX_INVITATION_BYTES, "invitation too large");
        let invitation: Invitation = serde_json::from_str(&json).context("not an invitation")?;
        invitation.validate()?;
        Ok(invitation)
    }

    pub fn parsed_routes(&self) -> Vec<Route> {
        self.routes
            .iter()
            .filter_map(|r| Route::parse(r).ok())
            .collect()
    }
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding.
pub fn b64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(B64URL[(n >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    out
}

/// Decodes base64url without padding; padding, whitespace and other alphabets are refused.
pub fn b64url_decode(text: &str) -> Result<Vec<u8>> {
    ensure!(text.len() % 4 != 1, "bad base64url length");
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in text.bytes() {
        let Some(v) = B64URL.iter().position(|&a| a == c) else {
            bail!("bad base64url character")
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    ensure!(acc == 0, "bad base64url padding bits");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../clients/shared/fixtures/phone-pairing-v2.json"
    );

    #[test]
    fn codes_are_twelve_symbols_and_normalize() {
        let code = new_code();
        assert_eq!(code.len(), 12);
        assert_eq!(normalize_code(&code).as_deref(), Some(code.as_str()));
        assert_eq!(
            normalize_code("abcd-efgh-jk23").as_deref(),
            Some("ABCDEFGHJK23")
        );
        assert_eq!(
            normalize_code(" AB CD EF GH JK 23 ").as_deref(),
            Some("ABCDEFGHJK23")
        );
        assert_eq!(normalize_code("ABCDEFGHJK2"), None, "11 symbols");
        assert_eq!(
            normalize_code("ABCDEFGHJK230"),
            None,
            "0 is not in the alphabet"
        );
        assert_eq!(
            normalize_code("ABCDEFGHIK23"),
            None,
            "I is not in the alphabet"
        );
        assert_eq!(display_code("ABCDEFGHJK23"), "ABCD-EFGH-JK23");
    }

    #[test]
    fn base64url_round_trips_and_refuses_padding() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64url_decode(&b64url_encode(&bytes)).unwrap(), bytes);
        }
        assert!(b64url_decode("AA==").is_err());
        assert!(b64url_decode("A+").is_err());
        assert!(b64url_decode("AB").is_err(), "nonzero padding bits");
        assert_eq!(b64url_encode(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn routes_parse_and_print() {
        let cases = [
            ("tls:192.168.1.20:47321", "tls:192.168.1.20:47321"),
            ("tls:[2001:db8::5]:47321", "tls:[2001:db8::5]:47321"),
            ("tls:Box.Tail1234.ts.net:443", "tls:box.tail1234.ts.net:443"),
            ("relay:whatever-phase-2-says", "relay:whatever-phase-2-says"),
        ];
        for (input, printed) in cases {
            assert_eq!(Route::parse(input).unwrap().to_string(), printed, "{input}");
        }
        for bad in [
            "tls:1.2.3.4",
            "tls:1.2.3.4:0",
            "tls:[::1:47321",
            "tls:-bad-.example:1",
            "tls:12345:1",
            ":x",
            "TLS:1.2.3.4:1",
        ] {
            assert!(Route::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn proofs_check_and_refuse_other_keys() {
        let key = proof_key("ABCD-EFGH-JK23").unwrap();
        let pin = pin_bytes(b"spki");
        let nonce = [7u8; 32];
        let proof = pin_proof(&key, &pin, &nonce, "id").unwrap();
        assert!(verify_pin_proof(&key, &pin, &nonce, "id", &proof));
        assert!(!verify_pin_proof(
            &key,
            &pin_bytes(b"other"),
            &nonce,
            "id",
            &proof
        ));
        assert!(!verify_pin_proof(&key, &pin, &[8u8; 32], "id", &proof));
        assert!(!verify_pin_proof(&key, &pin, &nonce, "id2", &proof));
        assert!(pin_proof(&key, &pin, &[0u8; 16], "id").is_err());
        let other = proof_key("ABCD-EFGH-JK24").unwrap();
        assert!(!verify_pin_proof(&other, &pin, &nonce, "id", &proof));
    }

    #[test]
    fn invitations_round_trip_and_refuse_extras() {
        let invitation = vector_invitation();
        let link = invitation.to_link();
        assert!(link.starts_with(LINK_PREFIX));
        assert_eq!(Invitation::parse(&link).unwrap(), invitation);
        assert_eq!(
            Invitation::parse(&invitation.to_json()).unwrap(),
            invitation
        );
        let mut extra: serde_json::Value = serde_json::from_str(&invitation.to_json()).unwrap();
        extra["url"] = "https://x".into();
        assert!(Invitation::parse(&extra.to_string()).is_err());
        let mut v1 = extra.clone();
        v1["version"] = 1.into();
        assert!(Invitation::parse(&v1.to_string()).is_err());
        let mut no_routes = invitation.clone();
        no_routes.routes.clear();
        assert!(no_routes.validate().is_err());
    }

    fn vector_invitation() -> Invitation {
        Invitation::new(
            "8b0f5c7e-2a44-4b8e-9d21-6a4e1c3f7b90",
            "box",
            &pin(VECTOR_SPKI),
            Some("ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6".into()),
            "ABCD-EFGH-JK23",
            1_763_900_000_000,
            &[
                Route::parse("tls:192.168.1.20:47321").unwrap(),
                Route::parse("tls:[2001:db8::5]:47321").unwrap(),
                Route::parse("tls:box.tail1234.ts.net:47321").unwrap(),
            ],
            &["read".into(), "operate".into()],
        )
        .unwrap()
    }

    /// A fixed P-256 SubjectPublicKeyInfo (generated once; any 91-byte SPKI would do).
    const VECTOR_SPKI: &[u8] = &[
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04, 0x6b, 0x17, 0xd1,
        0xf2, 0xe1, 0x2c, 0x42, 0x47, 0xf8, 0xbc, 0xe6, 0xe5, 0x63, 0xa4, 0x40, 0xf2, 0x77, 0x03,
        0x7d, 0x81, 0x2d, 0xeb, 0x33, 0xa0, 0xf4, 0xa1, 0x39, 0x45, 0xd8, 0x98, 0xc2, 0x96, 0x4f,
        0xe3, 0x42, 0xe2, 0xfe, 0x1a, 0x7f, 0x9b, 0x8e, 0xe7, 0xeb, 0x4a, 0x7c, 0x0f, 0x9e, 0x16,
        0x2b, 0xce, 0x33, 0x57, 0x6b, 0x31, 0x5e, 0xce, 0xcb, 0xb6, 0x40, 0x68, 0x37, 0xbf, 0x51,
        0xf5,
    ];

    fn vectors() -> serde_json::Value {
        let code = "ABCDEFGHJK23";
        let key = proof_key(code).unwrap();
        let pin_raw = pin_bytes(VECTOR_SPKI);
        let nonce: Vec<u8> = (0u8..32).collect();
        let gateway_id = "8b0f5c7e-2a44-4b8e-9d21-6a4e1c3f7b90";
        let proof = pin_proof(&key, &pin_raw, &nonce, gateway_id).unwrap();
        let invitation = vector_invitation();
        serde_json::json!({
            "comment": "Generated by `cargo test -p twr-core phone_pairing`; set TWR_WRITE_VECTORS=1 to rewrite. Contract: docs/design/phone-pairing.md.",
            "argon2id": {"m_kib": 65536, "t": 3, "p": 1, "salt": "twr-mobile-pair-v2", "len": 32},
            "hkdf": {"salt": "twr-mobile-pair-v2", "info": "pin-proof"},
            "codes": {
                "valid": [["abcd-efgh-jk23", "ABCDEFGHJK23"], [" AB CD EF GH JK 23 ", "ABCDEFGHJK23"]],
                "invalid": ["ABCDEFGHJK2", "ABCDEFGHJK230", "ABCDEFGHIK23", "ABCD_EFGH_JK23", ""]
            },
            "spki_hex": hex::encode(VECTOR_SPKI),
            "pin": b64url_encode(&pin_raw),
            "code": code,
            "stretched_hex": hex::encode(stretch(code).unwrap()),
            "proof_key_hex": hex::encode(key),
            "nonce_b64url": b64url_encode(&nonce),
            "gateway_id": gateway_id,
            "proof_b64url": b64url_encode(&proof),
            "invitation_json": invitation.to_json(),
            "invitation_link": invitation.to_link(),
            "routes": {
                "valid": [
                    ["tls:192.168.1.20:47321", {"kind": "tls", "host": "192.168.1.20", "port": 47321}],
                    ["tls:[2001:db8::5]:47321", {"kind": "tls", "host": "2001:db8::5", "port": 47321}],
                    ["tls:Box.Tail1234.ts.net:443", {"kind": "tls", "host": "box.tail1234.ts.net", "port": 443}],
                    ["relay:anything", {"kind": "other"}]
                ],
                "invalid": ["tls:1.2.3.4", "tls:1.2.3.4:0", "tls:[::1:47321", "tls:-bad-.example:1", "tls:12345:1", ":x", "TLS:1.2.3.4:1"]
            }
        })
    }

    #[test]
    fn vectors_match_the_fixture() {
        let generated = serde_json::to_string_pretty(&vectors()).unwrap() + "\n";
        if std::env::var_os("TWR_WRITE_VECTORS").is_some() {
            std::fs::write(FIXTURE, &generated).unwrap();
        }
        let on_disk = std::fs::read_to_string(FIXTURE)
            .expect("fixture missing: run with TWR_WRITE_VECTORS=1");
        assert_eq!(
            on_disk, generated,
            "fixture is stale: rerun with TWR_WRITE_VECTORS=1"
        );
    }
}
