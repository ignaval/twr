//! Code authentication bound to the actual QUIC exporter and both full identities.
//! The public signed locator already permits offline guessing. A PAKE cannot remove that
//! verifier; this protocol uses ring HMAC rather than adding unaudited curve arithmetic.
//! 60 random bits, Argon2id 64 MiB/t=3, fifteen-minute expiry, three admitted failures.
use std::{fmt, time::Instant};

use anyhow::{ensure, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use ring::{hkdf, hmac};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn same_secret(a: &str, b: &str) -> bool {
    let key = hmac::Key::new(
        hmac::HMAC_SHA256,
        b"twr constant-time receipt comparison v1",
    );
    hmac::verify(&key, a.as_bytes(), hmac::sign(&key, b.as_bytes()).as_ref()).is_ok()
}

pub struct Code(String);

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Code(<redacted>)")
    }
}

impl Code {
    pub fn generate() -> Result<Self> {
        let mut bits = u64::from_le_bytes(crate::random()?);
        let code = (0..12)
            .map(|_| {
                let character = ALPHABET[(bits & 31) as usize] as char;
                bits >>= 5;
                character
            })
            .collect();
        Ok(Self(code))
    }

    pub fn parse(input: &str) -> Result<Self> {
        ensure!(input.len() <= 64, "invalid pairing code");
        let normalized: String = input
            .bytes()
            .filter(|b| *b != b'-' && *b != b' ')
            .map(|b| b.to_ascii_uppercase() as char)
            .collect();
        ensure!(
            normalized.len() == 12 && normalized.bytes().all(|b| ALPHABET.contains(&b)),
            "invalid pairing code"
        );
        Ok(Self(normalized))
    }

    /// Deliberate user-only presentation; never Debug, events or logs.
    pub fn display(&self) -> String {
        format!("{}-{}-{}", &self.0[..4], &self.0[4..8], &self.0[8..])
    }

    pub fn derive(&self) -> Result<Derived> {
        let parameters = Params::new(64 * 1024, 3, 1, Some(32))
            .map_err(|_| anyhow::anyhow!("invalid Argon2 parameters"))?;
        let mut stretched = [0; 32];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, parameters)
            .hash_password_into(self.0.as_bytes(), b"twr-pair-v1", &mut stretched)
            .map_err(|_| anyhow::anyhow!("pairing KDF failed"))?;
        Ok(Derived {
            locator: expand(&stretched, b"locate")?,
            password: expand(&stretched, b"confirmation")?,
        })
    }
}

fn expand(input: &[u8], label: &[u8]) -> Result<[u8; 32]> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"twr-pair-v1");
    let prk = salt.extract(input);
    let info = [label];
    let output = prk
        .expand(&info, hkdf::HKDF_SHA256)
        .map_err(|_| anyhow::anyhow!("pairing HKDF failed"))?;
    let mut bytes = [0; 32];
    output
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("pairing HKDF failed"))?;
    Ok(bytes)
}

pub struct Derived {
    locator: [u8; 32],
    password: [u8; 32],
}

impl Derived {
    pub fn locator_key(&self) -> pkarr::Keypair {
        pkarr::Keypair::from_secret_key(&self.locator)
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transcript {
    #[serde(default)]
    pub replace: bool,
    pub version: u32,
    pub hub: String,
    pub runner: String,
    pub locator: String,
    pub session: String,
    pub expires: u64,
    pub certificate: crate::identity::Certificate,
}
impl Transcript {
    pub fn validate(
        &self,
        derived: &Derived,
        hub: iroh::EndpointId,
        runner: iroh::EndpointId,
        session: &str,
        expires: u64,
    ) -> Result<()> {
        ensure!(
            self.version == 1 && self.hub == hub.to_string() && self.runner == runner.to_string(),
            "wrong pairing roles or endpoints"
        );
        ensure!(
            self.locator == derived.locator_key().public_key().to_string()
                && self.session == session
                && self.expires == expires
                && expires > crate::now(),
            "stale or incorrect pairing session"
        );
        self.certificate.verify(hub)?;
        Ok(())
    }
    pub fn proof(&self, derived: &Derived, exporter: &[u8; 32], role: &str) -> Result<String> {
        ensure!(role == "hub" || role == "runner", "invalid pairing role");
        let mut bytes = b"twr-pair-quic-hmac-sha256-v1\0".to_vec();
        bytes.extend(exporter);
        bytes.extend(role.as_bytes());
        bytes.push(0);
        bytes.extend(serde_json::to_vec(self)?);
        let key = hmac::Key::new(hmac::HMAC_SHA256, &derived.password);
        Ok(hex::encode(hmac::sign(&key, &bytes)))
    }
    pub fn verify(
        &self,
        derived: &Derived,
        exporter: &[u8; 32],
        role: &str,
        proof: &str,
    ) -> Result<()> {
        let expected = hex::decode(self.proof(derived, exporter, role)?)?;
        let received = hex::decode(proof)?;
        ensure!(
            constant_time(&expected, &received),
            "pairing confirmation failed"
        );
        Ok(())
    }
}
fn constant_time(a: &[u8], b: &[u8]) -> bool {
    // ring's HMAC verifier provides constant-time equality for the authentication value.
    let key = hmac::Key::new(hmac::HMAC_SHA256, b"twr confirmation equality");
    hmac::verify(&key, a, hmac::sign(&key, b).as_ref()).is_ok()
}
/// Admission is checked before the expensive handshake; one serialized winner consumes it.
pub struct Window {
    pub session: String,
    pub admission: String,
    pub expires: u64,
    failures: u8,
    consumed: bool,
    deadline: Instant,
}
impl Window {
    pub fn new() -> Result<Self> {
        Ok(Self {
            session: hex::encode(crate::random::<16>()?),
            admission: hex::encode(crate::random::<16>()?),
            expires: crate::now() + 900,
            failures: 0,
            consumed: false,
            deadline: Instant::now() + std::time::Duration::from_secs(900),
        })
    }
    pub fn admit(&self, session: &str, admission: &str) -> Result<()> {
        ensure!(
            !self.consumed
                && self.failures < 3
                && Instant::now() < self.deadline
                && crate::now() < self.expires,
            "pairing window closed"
        );
        ensure!(
            constant_time(self.session.as_bytes(), session.as_bytes())
                && constant_time(self.admission.as_bytes(), admission.as_bytes()),
            "unknown pairing session"
        );
        Ok(())
    }
    pub fn failed(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }
    pub fn consume(&mut self) -> Result<()> {
        self.admit(&self.session, &self.admission)?;
        self.consumed = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transcript_rejects_wrong_code_roles_exporter_payload_and_replay() -> Result<()> {
        let derived = Code::parse("4HK7-M2QP-9XTR")?.derive()?;
        let wrong = Code::parse("4HK7-M2QP-9XTS")?.derive()?;
        let hub = iroh::SecretKey::generate().public();
        let runner = iroh::SecretKey::generate().public();
        let cert = crate::identity::Certificate::issue(
            &twr_core::sealed::HubKey::generate()?,
            &twr_core::sealed::HubKey::generate()?,
            hub,
            "01".repeat(16),
            "02".repeat(16),
        )?;
        let t = Transcript {
            version: 1,
            replace: false,
            hub: hub.to_string(),
            runner: runner.to_string(),
            locator: derived.locator_key().public_key().to_string(),
            session: "03".repeat(16),
            expires: crate::now() + 900,
            certificate: cert,
        };
        t.validate(&derived, hub, runner, &t.session, t.expires)?;
        let proof = t.proof(&derived, &[1; 32], "hub")?;
        t.verify(&derived, &[1; 32], "hub", &proof)?;
        assert!(t.verify(&wrong, &[1; 32], "hub", &proof).is_err());
        assert!(t.verify(&derived, &[1; 32], "runner", &proof).is_err());
        assert!(t.verify(&derived, &[2; 32], "hub", &proof).is_err());
        let mut changed = t.clone();
        changed.certificate.binding.generation = "04".repeat(16);
        assert!(changed.verify(&derived, &[1; 32], "hub", &proof).is_err());
        assert!(t
            .validate(&derived, runner, hub, &t.session, t.expires)
            .is_err());
        assert!(t
            .validate(&derived, hub, runner, &"ff".repeat(16), t.expires)
            .is_err());
        assert!(t
            .validate(&derived, hub, runner, &t.session, crate::now())
            .is_err());
        Ok(())
    }
    #[test]
    fn admission_is_bounded_and_single_use() -> Result<()> {
        let mut window = Window::new()?;
        assert!(window.admit(&window.session, "unknown").is_err());
        for _ in 0..3 {
            window.admit(&window.session, &window.admission)?;
            window.failed();
        }
        assert!(window.admit(&window.session, &window.admission).is_err());
        let mut window = Window::new()?;
        window.consume()?;
        assert!(window.consume().is_err());
        assert!(!format!("{:?}", Code::parse("4HK7-M2QP-9XTR")?).contains("4HK7"));
        Ok(())
    }
}
