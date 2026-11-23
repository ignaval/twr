//! The gateway's own identity, which paired phones pin (docs/design/phone-pairing.md#identity):
//! a P-256 key, a self-signed certificate for it, and an iroh key for phase 2. All three live
//! in the gateway's directory (`mobile/`, 0700), each 0600, made atomically and read only as
//! private regular files of this user.
use anyhow::{Context, Result};
use std::{
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

/// The identity key: ECDSA P-256, PKCS#8 PEM.
pub const KEY_FILE: &str = "identity-key.pem";
/// The self-signed certificate of [`KEY_FILE`].
pub const CERT_FILE: &str = "identity-cert.pem";
/// 32 random bytes: the iroh endpoint's Ed25519 secret key (unused in phase 1).
pub const IROH_FILE: &str = "iroh-key";
/// A certificate's validity, from an hour before it is made.
const VALIDITY_DAYS: i64 = 800;
/// A certificate is made again, for the same key, once fewer days than this remain.
const REISSUE_DAYS: i64 = 30;
/// The largest identity file read.
const MAX_FILE: u64 = 16 * 1024;

/// The identity as the gateway serves it.
pub struct Identity {
    pub key_pem: String,
    pub cert_pem: String,
    /// The pin: base64url of the SHA-256 of the key's SubjectPublicKeyInfo.
    pub pin: String,
    /// The pin's 32 bytes.
    pub pin_raw: [u8; 32],
    /// The iroh `EndpointId` of [`IROH_FILE`].
    pub iroh: String,
    /// When the certificate stops being valid, in seconds since the epoch.
    pub not_after: i64,
}

impl Identity {
    /// The identity in `dir`, making what is missing: the key (once), the iroh key (once) and
    /// the certificate (again, for the same key, when it is missing, not this key's, or fewer
    /// than 30 days remain).
    pub fn ensure(dir: &Path) -> Result<Self> {
        crate::remote::gateway_lifecycle::private_dir(dir)?;
        let key_pem = match read(dir, KEY_FILE)? {
            Some(pem) => pem,
            None => {
                let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
                    .context("making the gateway's identity key")?;
                create(dir, KEY_FILE, key.serialize_pem().as_bytes())?;
                // Another process may have made one first: that one is the key.
                read(dir, KEY_FILE)?.context("the identity key vanished")?
            }
        };
        if read_bytes(dir, IROH_FILE)?.is_none() {
            create(dir, IROH_FILE, &random::<32>())?;
        }
        let key = rcgen::KeyPair::from_pem(&key_pem).context("reading the identity key")?;
        let spki = rcgen::PublicKeyData::subject_public_key_info(&key);
        let current = read(dir, CERT_FILE)?.and_then(|pem| {
            let not_after = certificate_of(&pem, &spki)?;
            (not_after - now() >= REISSUE_DAYS * 86400).then_some((pem, not_after))
        });
        let (cert_pem, not_after) = match current {
            Some(current) => current,
            None => {
                let (pem, not_after) = issue(&key)?;
                replace(dir, CERT_FILE, pem.as_bytes())?;
                (pem, not_after)
            }
        };
        Self::of(dir, key_pem, cert_pem, &spki, not_after)
    }

    /// The identity in `dir` as it is, making nothing: `None` without a key or certificate.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let (Some(key_pem), Some(cert_pem)) = (read(dir, KEY_FILE)?, read(dir, CERT_FILE)?) else {
            return Ok(None);
        };
        let key = rcgen::KeyPair::from_pem(&key_pem).context("reading the identity key")?;
        let spki = rcgen::PublicKeyData::subject_public_key_info(&key);
        let not_after = certificate_of(&cert_pem, &spki)
            .context("the identity certificate is not its key's")?;
        Self::of(dir, key_pem, cert_pem, &spki, not_after).map(Some)
    }

    /// A new identity in `dir`, replacing the old one (`twr phone rekey`): a new key,
    /// certificate and iroh key. Every phone that pinned the old key must pair again.
    pub fn rekey(dir: &Path) -> Result<Self> {
        crate::remote::gateway_lifecycle::private_dir(dir)?;
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .context("making the gateway's identity key")?;
        let (cert, _) = issue(&key)?;
        replace(dir, IROH_FILE, &random::<32>())?;
        replace(dir, KEY_FILE, key.serialize_pem().as_bytes())?;
        replace(dir, CERT_FILE, cert.as_bytes())?;
        Self::ensure(dir)
    }

    fn of(
        dir: &Path,
        key_pem: String,
        cert_pem: String,
        spki: &[u8],
        not_after: i64,
    ) -> Result<Self> {
        let iroh = read_bytes(dir, IROH_FILE)?.context("the iroh key is missing")?;
        let iroh: [u8; 32] = iroh
            .try_into()
            .map_err(|_| anyhow::anyhow!("the iroh key is not 32 bytes"))?;
        Ok(Self {
            key_pem,
            cert_pem,
            pin: twr_core::phone_pairing::pin(spki),
            pin_raw: twr_core::phone_pairing::pin_bytes(spki),
            iroh: twr_link::identity::endpoint_id(&iroh),
            not_after,
        })
    }

    /// Whether the certificate is due to be made again.
    pub fn due(&self) -> bool {
        self.not_after - now() < REISSUE_DAYS * 86400
    }
}

/// A certificate for `key`: CN `twr hub`, SAN `DNS:twr-hub.invalid`, EKU serverAuth, valid
/// 800 days from an hour ago; its PEM and when it ends.
fn issue(key: &rcgen::KeyPair) -> Result<(String, i64)> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose};
    let mut params = CertificateParams::new(vec!["twr-hub.invalid".to_string()])?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "twr hub");
    params.distinguished_name = name;
    let start = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    params.not_before = start;
    params.not_after = start + time::Duration::days(VALIDITY_DAYS);
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let mut serial = random::<16>();
    serial[0] &= 0x7f;
    params.serial_number = Some(serial.to_vec().into());
    let cert = params.self_signed(key)?;
    Ok((cert.pem(), params.not_after.unix_timestamp()))
}

/// When certificate `pem` ends, if it is one certificate for the key whose SubjectPublicKeyInfo
/// is `spki`.
fn certificate_of(pem: &str, spki: &[u8]) -> Option<i64> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let der = CertificateDer::from_pem_slice(pem.as_bytes()).ok()?;
    let (not_after, key) = der::validity_and_spki(&der)?;
    (key == spki).then_some(not_after)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn random<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .expect("OS entropy");
    bytes
}

/// `dir/name` as text, when it exists.
fn read(dir: &Path, name: &str) -> Result<Option<String>> {
    read_bytes(dir, name)?
        .map(|bytes| String::from_utf8(bytes).with_context(|| format!("{name} is not text")))
        .transpose()
}

/// `dir/name`, when it exists: a regular file of this user's, private (0600 or less), not a
/// symbolic link.
fn read_bytes(dir: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let path = dir.join(name);
    match twr_link::state::read_private(&path, MAX_FILE) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.context(format!(
            "{} must be a private file of yours",
            path.display()
        ))),
    }
}

/// A private temporary file in `dir` holding `bytes`, synced.
fn temporary(dir: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix(".identity-")
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(dir)?;
    let meta = file.as_file().metadata()?;
    anyhow::ensure!(
        meta.uid() == twr_core::paths::effective_uid() && meta.mode() & 0o077 == 0,
        "the gateway's directory makes files others can read"
    );
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    Ok(file)
}

/// Writes `dir/name` once: when it exists already, it stays (and wins).
fn create(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    match temporary(dir, bytes)?.persist_noclobber(dir.join(name)) {
        Ok(_) => {}
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.error.into()),
    }
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Writes `dir/name` in place of what is there, atomically.
fn replace(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    temporary(dir, bytes)?
        .persist(dir.join(name))
        .map_err(|e| e.error)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Just enough DER to read a certificate's end of validity and its SubjectPublicKeyInfo.
mod der {
    /// The tag, contents and rest of the first element of `input`.
    fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&tag, rest) = input.split_first()?;
        let (&first, rest) = rest.split_first()?;
        let (len, rest) = if first < 0x80 {
            (first as usize, rest)
        } else {
            let n = (first & 0x7f) as usize;
            if n == 0 || n > 4 || rest.len() < n {
                return None;
            }
            let len = rest[..n].iter().fold(0usize, |l, b| (l << 8) | *b as usize);
            (len, &rest[n..])
        };
        (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
    }

    /// The whole first element of `input`, tag and length included.
    fn whole(input: &[u8]) -> Option<&[u8]> {
        let (_, _, rest) = element(input)?;
        Some(&input[..input.len() - rest.len()])
    }

    /// A certificate's notAfter (seconds since the epoch) and its SubjectPublicKeyInfo DER.
    pub fn validity_and_spki(cert: &[u8]) -> Option<(i64, &[u8])> {
        let (0x30, certificate, _) = element(cert)? else {
            return None;
        };
        let (0x30, tbs, _) = element(certificate)? else {
            return None;
        };
        let mut rest = tbs;
        if rest.first() == Some(&0xa0) {
            rest = element(rest)?.2;
        }
        let rest = element(rest)?.2; // serialNumber
        let rest = element(rest)?.2; // signature
        let rest = element(rest)?.2; // issuer
        let (0x30, validity, rest) = element(rest)? else {
            return None;
        };
        let rest = element(rest)?.2; // subject
        let spki = whole(rest)?;
        let after = element(validity)?.2;
        let (tag, time, _) = element(after)?;
        Some((time_of(tag, time)?, spki))
    }

    /// UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`), in seconds.
    fn time_of(tag: u8, text: &[u8]) -> Option<i64> {
        let text = std::str::from_utf8(text).ok()?.strip_suffix('Z')?;
        let (year, rest) = match (tag, text.len()) {
            (0x17, 12) => {
                let yy: i64 = text[..2].parse().ok()?;
                (if yy < 50 { 2000 + yy } else { 1900 + yy }, &text[2..])
            }
            (0x18, 14) => (text[..4].parse().ok()?, &text[4..]),
            _ => return None,
        };
        let field = |i: usize| -> Option<i64> { rest.get(i..i + 2)?.parse().ok() };
        let (month, day, hour, minute, second) =
            (field(0)?, field(2)?, field(4)?, field(6)?, field(8)?);
        // Days from the civil date (Howard Hinnant's algorithm).
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146097 + doe - 719468;
        Some(days * 86400 + hour * 3600 + minute * 60 + second)
    }

    #[cfg(test)]
    #[test]
    fn times_read_as_seconds() {
        assert_eq!(time_of(0x17, b"700101000000Z"), Some(0));
        assert_eq!(time_of(0x17, b"251123120000Z"), Some(1_763_899_200));
        assert_eq!(time_of(0x18, b"20510101000000Z"), Some(2_556_143_999 + 1));
        assert_eq!(time_of(0x17, b"2511231200Z"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    /// The identity is made once, private, and read back the same; its certificate is the
    /// contract's (800 days from an hour ago, `twr hub`, `twr-hub.invalid`, serverAuth) and
    /// carries its key.
    #[test]
    fn an_identity_is_made_once_and_kept_private() {
        let dir = private_dir();
        assert!(Identity::load(dir.path()).unwrap().is_none());
        let made = Identity::ensure(dir.path()).unwrap();
        for name in [KEY_FILE, CERT_FILE, IROH_FILE] {
            assert_eq!(mode(&dir.path().join(name)), 0o600, "{name}");
        }
        assert!(
            made.key_pem.starts_with("-----BEGIN PRIVATE KEY-----"),
            "PKCS#8"
        );
        assert_eq!(made.pin.len(), 43);
        assert_eq!(made.iroh.len(), 64, "an EndpointId is printed as hex");
        assert_eq!(std::fs::read(dir.path().join(IROH_FILE)).unwrap().len(), 32);
        let days = (made.not_after - now()) as f64 / 86400.0;
        assert!((799.9..800.0).contains(&days), "{days}");
        let again = Identity::ensure(dir.path()).unwrap();
        assert_eq!(
            (&again.pin, &again.iroh, &again.cert_pem),
            (&made.pin, &made.iroh, &made.cert_pem)
        );
        let loaded = Identity::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.pin, made.pin);
        // What a TLS client sees: the leaf's SPKI hashes to the pin.
        use rustls::pki_types::{pem::PemObject, CertificateDer};
        let der = CertificateDer::from_pem_slice(made.cert_pem.as_bytes()).unwrap();
        let (_, spki) = der::validity_and_spki(&der).unwrap();
        assert_eq!(twr_core::phone_pairing::pin(spki), made.pin);
        let text = String::from_utf8_lossy(&der);
        assert!(text.contains("twr hub") && text.contains("twr-hub.invalid"));
        // serverAuth: 1.3.6.1.5.5.7.3.1.
        let server_auth = [0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
        assert!(der.windows(server_auth.len()).any(|w| w == server_auth));
        // rustls takes the pair.
        let _ = rustls::crypto::ring::default_provider().install_default();
        use rustls::pki_types::PrivateKeyDer;
        let key = PrivateKeyDer::from_pem_slice(made.key_pem.as_bytes()).unwrap();
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key)
            .unwrap();
    }

    /// A certificate with fewer than 30 days left, or another key's, is made again for the
    /// same key: the pin stays.
    #[test]
    fn a_certificate_near_its_end_is_reissued_for_the_same_key() {
        let dir = private_dir();
        let made = Identity::ensure(dir.path()).unwrap();
        let key = rcgen::KeyPair::from_pem(&made.key_pem).unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["twr-hub.invalid".into()]).unwrap();
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(790);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(10);
        let old = params.self_signed(&key).unwrap().pem();
        replace(dir.path(), CERT_FILE, old.as_bytes()).unwrap();
        let loaded = Identity::load(dir.path()).unwrap().unwrap();
        assert!(loaded.due());
        let renewed = Identity::ensure(dir.path()).unwrap();
        assert_ne!(renewed.cert_pem, old);
        assert_eq!(renewed.pin, made.pin);
        assert_eq!(renewed.key_pem, made.key_pem);
        assert!(!renewed.due());
        // Another key's certificate.
        let other = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let (foreign, _) = issue(&other).unwrap();
        replace(dir.path(), CERT_FILE, foreign.as_bytes()).unwrap();
        assert!(Identity::load(dir.path()).is_err());
        let renewed = Identity::ensure(dir.path()).unwrap();
        assert_eq!(renewed.pin, made.pin);
        assert_ne!(renewed.cert_pem, foreign);
    }

    /// Files others can read, or links, are refused, not used or replaced.
    #[test]
    fn identity_files_must_be_private_regular_files() {
        let dir = private_dir();
        Identity::ensure(dir.path()).unwrap();
        let key = dir.path().join(KEY_FILE);
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Identity::ensure(dir.path()).is_err());
        assert!(Identity::load(dir.path()).is_err());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let elsewhere = private_dir();
        let target = elsewhere.path().join("key.pem");
        std::fs::rename(&key, &target).unwrap();
        std::os::unix::fs::symlink(&target, &key).unwrap();
        assert!(Identity::ensure(dir.path()).is_err());
        // A directory others can enter is refused too.
        let open = tempfile::tempdir().unwrap();
        std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Identity::ensure(open.path()).is_err());
    }

    /// Rekeying replaces all three: a new pin and iroh id.
    #[test]
    fn rekeying_makes_a_new_identity() {
        let dir = private_dir();
        let old = Identity::ensure(dir.path()).unwrap();
        let new = Identity::rekey(dir.path()).unwrap();
        assert_ne!(new.pin, old.pin);
        assert_ne!(new.iroh, old.iroh);
        assert_eq!(Identity::load(dir.path()).unwrap().unwrap().pin, new.pin);
        for name in [KEY_FILE, CERT_FILE, IROH_FILE] {
            assert_eq!(mode(&dir.path().join(name)), 0o600, "{name}");
        }
    }
}
