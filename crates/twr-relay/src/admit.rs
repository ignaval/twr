//! iroh admission records (docs/relay-protocol.md §10.2): building and signing them on the
//! client, checking them on the relay.

use crate::keys::{self, IdentityKey};
use crate::names;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const ADMIT_VERSIONS: &[u32] = &[1];
const PREFIX: &[u8] = b"twr-relay admit v1\0";
/// How far a pairing admission's `issued` may be from the relay's clock.
pub const PAIRING_SKEW_SECS: u64 = 300;
/// How long a pairing admission lasts.
pub const PAIRING_SECS: u64 = 15 * 60;
/// The longest a pair admission lasts per post.
pub const PAIR_MAX_SECS: u64 = 30 * 24 * 3600;
pub const MAX_BODY: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Admission {
    Pairing {
        v: u32,
        endpoint: String,
        issued: u64,
        sig: String,
    },
    Pair {
        v: u32,
        a: String,
        b: String,
        expires: u64,
        sig_a: String,
        sig_b: String,
    },
}

/// The bytes an endpoint signs for a pairing admission.
pub fn pairing_message(endpoint: &[u8; 32], issued: u64) -> Vec<u8> {
    [PREFIX, b"pairing\0", endpoint, &issued.to_be_bytes()].concat()
}

/// The bytes each endpoint signs for a pair admission.
pub fn pair_message(a: &[u8; 32], b: &[u8; 32], expires: u64) -> Vec<u8> {
    [PREFIX, b"pair\0", a, b, &expires.to_be_bytes()].concat()
}

pub fn parse_endpoint(hex_text: &str) -> Option<[u8; 32]> {
    if hex_text.len() != 64 || hex_text.bytes().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    hex::decode(hex_text).ok()?.try_into().ok()
}

fn endpoint_of(key: &IdentityKey) -> Result<[u8; 32]> {
    if key.kind() != keys::KeyKind::Ed25519 {
        bail!("iroh endpoint keys are Ed25519");
    }
    Ok(key.public_raw().try_into()?)
}

/// A signed pairing admission for `key`, an iroh endpoint's secret key.
pub fn sign_pairing(key: &IdentityKey, issued: u64) -> Result<Admission> {
    let ep = endpoint_of(key)?;
    let sig = key.sign(&pairing_message(&ep, issued))?;
    Ok(Admission::Pairing {
        v: 1,
        endpoint: hex::encode(ep),
        issued,
        sig: names::b64(&sig),
    })
}

/// One side's signature for a pair admission of endpoints `a` and `b`. Each side signs with
/// its own key; [`pair_admission`] puts the two together.
pub fn sign_pair_half(
    key: &IdentityKey,
    a: &[u8; 32],
    b: &[u8; 32],
    expires: u64,
) -> Result<String> {
    let me = endpoint_of(key)?;
    if &me != a && &me != b {
        bail!("this key is neither endpoint of the pair");
    }
    Ok(names::b64(&key.sign(&pair_message(a, b, expires))?))
}

pub fn pair_admission(
    a: &[u8; 32],
    b: &[u8; 32],
    expires: u64,
    sig_a: String,
    sig_b: String,
) -> Admission {
    Admission::Pair {
        v: 1,
        a: hex::encode(a),
        b: hex::encode(b),
        expires,
        sig_a,
        sig_b,
    }
}

/// What a verified admission admits: endpoints and the time they're admitted until.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    pub endpoints: Vec<[u8; 32]>,
    pub until: u64,
    pub pairing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    BadRequest,
    BadSignature,
    Stale,
}

impl Refused {
    pub fn code(&self) -> &'static str {
        match self {
            Refused::BadRequest => "bad_request",
            Refused::BadSignature => "bad_signature",
            Refused::Stale => "stale",
        }
    }
}

/// Checks a posted admission body at `now` (§10.2).
pub fn verify(body: &[u8], now: u64) -> std::result::Result<Admitted, Refused> {
    if body.len() > MAX_BODY {
        return Err(Refused::BadRequest);
    }
    let adm: Admission = serde_json::from_slice(body).map_err(|_| Refused::BadRequest)?;
    let sig = |s: &str| names::unb64(s).ok_or(Refused::BadRequest);
    match adm {
        Admission::Pairing {
            v,
            endpoint,
            issued,
            sig: s,
        } => {
            if v != 1 {
                return Err(Refused::BadRequest);
            }
            let ep = parse_endpoint(&endpoint).ok_or(Refused::BadRequest)?;
            if issued.abs_diff(now) > PAIRING_SKEW_SECS {
                return Err(Refused::Stale);
            }
            if !keys::verify_ed25519(&ep, &pairing_message(&ep, issued), &sig(&s)?) {
                return Err(Refused::BadSignature);
            }
            Ok(Admitted {
                endpoints: vec![ep],
                until: now + PAIRING_SECS,
                pairing: true,
            })
        }
        Admission::Pair {
            v,
            a,
            b,
            expires,
            sig_a,
            sig_b,
        } => {
            if v != 1 {
                return Err(Refused::BadRequest);
            }
            let a = parse_endpoint(&a).ok_or(Refused::BadRequest)?;
            let b = parse_endpoint(&b).ok_or(Refused::BadRequest)?;
            if a == b {
                return Err(Refused::BadRequest);
            }
            if expires <= now {
                return Err(Refused::Stale);
            }
            let msg = pair_message(&a, &b, expires);
            if !keys::verify_ed25519(&a, &msg, &sig(&sig_a)?)
                || !keys::verify_ed25519(&b, &msg, &sig(&sig_b)?)
            {
                return Err(Refused::BadSignature);
            }
            Ok(Admitted {
                endpoints: vec![a, b],
                until: expires.min(now + PAIR_MAX_SECS),
                pairing: false,
            })
        }
    }
}

/// The answer body of a successful admission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmitAnswer {
    pub admitted: Vec<String>,
    pub until: u64,
}

pub fn parse_answer(body: &[u8]) -> Result<AdmitAnswer> {
    serde_json::from_slice(body).context("reading the admission answer")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_and_pair() {
        let now = 1_800_000_000;
        let a = IdentityKey::ed25519_from_seed(&[1; 32]).unwrap();
        let b = IdentityKey::ed25519_from_seed(&[2; 32]).unwrap();
        let body = serde_json::to_vec(&sign_pairing(&a, now - 10).unwrap()).unwrap();
        let ok = verify(&body, now).unwrap();
        assert_eq!(ok.until, now + PAIRING_SECS);
        assert_eq!(verify(&body, now + 400), Err(Refused::Stale));

        let ea: [u8; 32] = a.public_raw().try_into().unwrap();
        let eb: [u8; 32] = b.public_raw().try_into().unwrap();
        let exp = now + 400 * 86400;
        let adm = pair_admission(
            &ea,
            &eb,
            exp,
            sign_pair_half(&a, &ea, &eb, exp).unwrap(),
            sign_pair_half(&b, &ea, &eb, exp).unwrap(),
        );
        let body = serde_json::to_vec(&adm).unwrap();
        let ok = verify(&body, now).unwrap();
        assert_eq!(ok.endpoints, vec![ea, eb]);
        assert_eq!(ok.until, now + PAIR_MAX_SECS);
        // A swapped signature fails.
        let bad = pair_admission(
            &ea,
            &eb,
            exp,
            sign_pair_half(&a, &ea, &eb, exp).unwrap(),
            sign_pair_half(&a, &ea, &eb, exp).unwrap(),
        );
        assert_eq!(
            verify(&serde_json::to_vec(&bad).unwrap(), now),
            Err(Refused::BadSignature)
        );
        assert_eq!(verify(b"{}", now), Err(Refused::BadRequest));
    }
}
