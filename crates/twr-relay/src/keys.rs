//! Identity keys: SPKI parsing, signatures and the hub's signer (docs/relay-protocol.md §3.1).

use anyhow::{anyhow, bail, Result};
use ring::rand::SystemRandom;
use ring::signature::{self, EcdsaKeyPair, Ed25519KeyPair, KeyPair as _};

/// DER prefix of a P-256 SubjectPublicKeyInfo, before its 65-byte uncompressed point.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];
/// DER prefix of an Ed25519 SubjectPublicKeyInfo, before its 32-byte key.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    P256,
    Ed25519,
}

/// The key type and raw public key of an SPKI the protocol accepts, or `None`.
pub fn parse_spki(der: &[u8]) -> Option<(KeyKind, &[u8])> {
    if der.len() == 91 && der.starts_with(&P256_SPKI_PREFIX) && der[26] == 0x04 {
        return Some((KeyKind::P256, &der[26..]));
    }
    if der.len() == 44 && der.starts_with(&ED25519_SPKI_PREFIX) {
        return Some((KeyKind::Ed25519, &der[12..]));
    }
    None
}

/// The SPKI of a raw public key.
pub fn spki_of(kind: KeyKind, raw: &[u8]) -> Vec<u8> {
    let prefix: &[u8] = match kind {
        KeyKind::P256 => &P256_SPKI_PREFIX,
        KeyKind::Ed25519 => &ED25519_SPKI_PREFIX,
    };
    [prefix, raw].concat()
}

/// Whether `sig` is the SPKI key's signature over `msg` (§3.1's table).
pub fn verify(spki_der: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    let Some((kind, raw)) = parse_spki(spki_der) else {
        return false;
    };
    let alg: &dyn signature::VerificationAlgorithm = match kind {
        KeyKind::P256 => &signature::ECDSA_P256_SHA256_ASN1,
        KeyKind::Ed25519 => &signature::ED25519,
    };
    signature::UnparsedPublicKey::new(alg, raw)
        .verify(msg, sig)
        .is_ok()
}

/// Whether `sig` is the raw Ed25519 key's signature over `msg`.
pub fn verify_ed25519(raw: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    signature::UnparsedPublicKey::new(&signature::ED25519, raw)
        .verify(msg, sig)
        .is_ok()
}

/// A signing key: a hub's identity key, or an iroh endpoint key for admission.
pub enum IdentityKey {
    P256 {
        pair: EcdsaKeyPair,
        pkcs8: Vec<u8>,
    },
    Ed25519 {
        pair: Ed25519KeyPair,
        pkcs8: Option<Vec<u8>>,
    },
}

impl std::fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "IdentityKey({:?}, pin {})",
            self.kind(),
            crate::names::pin_text(&self.pin())
        )
    }
}

impl IdentityKey {
    /// A new random P-256 key.
    pub fn generate_p256() -> Result<Self> {
        let rng = SystemRandom::new();
        let doc = EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
            .map_err(|_| anyhow!("generating a P-256 key failed"))?;
        Self::from_pkcs8(doc.as_ref())
    }

    /// A new random Ed25519 key.
    pub fn generate_ed25519() -> Result<Self> {
        let rng = SystemRandom::new();
        let doc = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| anyhow!("generating an Ed25519 key failed"))?;
        Self::from_pkcs8(doc.as_ref())
    }

    /// An Ed25519 key from its 32-byte seed, as iroh stores endpoint secret keys.
    pub fn ed25519_from_seed(seed: &[u8; 32]) -> Result<Self> {
        let pair =
            Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| anyhow!("bad Ed25519 seed"))?;
        Ok(Self::Ed25519 { pair, pkcs8: None })
    }

    /// A P-256 or Ed25519 key from PKCS#8 DER.
    pub fn from_pkcs8(der: &[u8]) -> Result<Self> {
        let rng = SystemRandom::new();
        if let Ok(pair) =
            EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_ASN1_SIGNING, der, &rng)
        {
            return Ok(Self::P256 {
                pair,
                pkcs8: der.to_vec(),
            });
        }
        if let Ok(pair) = Ed25519KeyPair::from_pkcs8_maybe_unchecked(der) {
            return Ok(Self::Ed25519 {
                pair,
                pkcs8: Some(der.to_vec()),
            });
        }
        bail!("not a P-256 or Ed25519 PKCS#8 key")
    }

    /// A key from a PEM file's text (`PRIVATE KEY`, PKCS#8).
    pub fn from_pkcs8_pem(pem: &str) -> Result<Self> {
        use rustls::pki_types::{pem::PemObject, PrivatePkcs8KeyDer};
        let der = PrivatePkcs8KeyDer::from_pem_slice(pem.as_bytes())
            .map_err(|e| anyhow!("reading the PEM key: {e}"))?;
        Self::from_pkcs8(der.secret_pkcs8_der())
    }

    pub fn kind(&self) -> KeyKind {
        match self {
            Self::P256 { .. } => KeyKind::P256,
            Self::Ed25519 { .. } => KeyKind::Ed25519,
        }
    }

    /// The raw public key: a 65-byte point or 32 bytes.
    pub fn public_raw(&self) -> &[u8] {
        match self {
            Self::P256 { pair, .. } => pair.public_key().as_ref(),
            Self::Ed25519 { pair, .. } => pair.public_key().as_ref(),
        }
    }

    pub fn spki(&self) -> Vec<u8> {
        spki_of(self.kind(), self.public_raw())
    }

    pub fn pin(&self) -> [u8; 32] {
        crate::names::pin(&self.spki())
    }

    /// The PKCS#8 DER, if this key came from or was made as one.
    pub fn pkcs8(&self) -> Option<&[u8]> {
        match self {
            Self::P256 { pkcs8, .. } => Some(pkcs8),
            Self::Ed25519 { pkcs8, .. } => pkcs8.as_deref(),
        }
    }

    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        match self {
            Self::P256 { pair, .. } => Ok(pair
                .sign(&SystemRandom::new(), msg)
                .map_err(|_| anyhow!("signing failed"))?
                .as_ref()
                .to_vec()),
            Self::Ed25519 { pair, .. } => Ok(pair.sign(msg).as_ref().to_vec()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_both_kinds() {
        for key in [
            IdentityKey::generate_p256().unwrap(),
            IdentityKey::generate_ed25519().unwrap(),
        ] {
            let spki = key.spki();
            assert_eq!(parse_spki(&spki).unwrap().0, key.kind());
            let sig = key.sign(b"hello").unwrap();
            assert!(verify(&spki, b"hello", &sig));
            assert!(!verify(&spki, b"hellO", &sig));
            let again = IdentityKey::from_pkcs8(key.pkcs8().unwrap()).unwrap();
            assert_eq!(again.spki(), spki);
        }
    }

    #[test]
    fn spki_matches_rcgen_and_rejects_others() {
        // A P-256 SPKI with a compressed point, and wrong lengths, are refused.
        let key = IdentityKey::generate_p256().unwrap();
        let mut spki = key.spki();
        spki[26] = 0x02;
        assert!(parse_spki(&spki).is_none());
        assert!(parse_spki(&key.spki()[..90]).is_none());
        assert!(parse_spki(&[]).is_none());
    }

    #[test]
    fn seed_key_is_ed25519() {
        let k = IdentityKey::ed25519_from_seed(&[5u8; 32]).unwrap();
        assert_eq!(k.public_raw().len(), 32);
        let sig = k.sign(b"m").unwrap();
        assert!(verify_ed25519(
            k.public_raw().try_into().unwrap(),
            b"m",
            &sig
        ));
    }
}
