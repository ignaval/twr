//! Protocol experiments only. RustCrypto spake2 0.4.0 is not independently audited;
//! passing these tests is not approval to ship it or a cryptographic security proof.

use std::{fmt, time::Instant};

use anyhow::{ensure, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::{rngs::OsRng, RngCore};
use ring::{hkdf, hmac};
use spake2::{Ed25519Group, Identity, Password, Spake2};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub struct Code(String);

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Code(<redacted>)")
    }
}

impl Code {
    pub fn generate() -> Self {
        let mut bits = OsRng.next_u64();
        let code = (0..12)
            .map(|_| {
                let character = ALPHABET[(bits & 31) as usize] as char;
                bits >>= 5;
                character
            })
            .collect();
        Self(code)
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

    pub fn derive(&self) -> Result<Derived> {
        let parameters = Params::new(64 * 1024, 3, 1, Some(32))
            .map_err(|_| anyhow::anyhow!("invalid Argon2 parameters"))?;
        let mut stretched = [0; 32];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, parameters)
            .hash_password_into(self.0.as_bytes(), b"twr-pair-v1", &mut stretched)
            .map_err(|_| anyhow::anyhow!("pairing KDF failed"))?;
        Ok(Derived {
            locator: expand(&stretched, b"locate")?,
            password: expand(&stretched, b"pake")?,
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

#[derive(Clone, Copy, Debug)]
pub enum Role {
    Hub,
    Runner,
}

/// Fixed-width, domain-separated context; keys are actual authenticated endpoint ids.
#[derive(Clone)]
pub struct Context {
    pub hub: [u8; 32],
    pub runner: [u8; 32],
    pub initiator: Role,
    pub session: [u8; 16],
    pub expires_unix: u64,
    pub identity_payload_hash: [u8; 32],
}

impl Context {
    fn encode(&self, derived: &Derived) -> Vec<u8> {
        let mut bytes = b"twr-pair-spake2-ed25519-spike-v1\0".to_vec();
        bytes.extend(self.hub);
        bytes.extend(self.runner);
        bytes.extend(derived.locator_key().public_key().as_bytes());
        bytes.push(match self.initiator {
            Role::Hub => 0,
            Role::Runner => 1,
        });
        bytes.extend(self.session);
        bytes.extend(self.expires_unix.to_be_bytes());
        bytes.extend(self.identity_payload_hash);
        bytes
    }
}

pub struct Exchange {
    state: Spake2<Ed25519Group>,
    role: Role,
    context: Vec<u8>,
}

impl Exchange {
    pub fn start(role: Role, derived: &Derived, context: &Context) -> (Self, Vec<u8>) {
        let encoded = context.encode(derived);
        let mut hub = encoded.clone();
        hub.extend(b"hub");
        let mut runner = encoded.clone();
        runner.extend(b"runner");
        let password = Password::new(derived.password);
        let (state, message) = match role {
            Role::Hub => Spake2::start_a(&password, &Identity::new(&hub), &Identity::new(&runner)),
            Role::Runner => {
                Spake2::start_b(&password, &Identity::new(&hub), &Identity::new(&runner))
            }
        };
        (
            Self {
                state,
                role,
                context: encoded,
            },
            message,
        )
    }

    pub fn finish(self, peer: &[u8]) -> Result<Unconfirmed> {
        ensure!(peer.len() == 33, "invalid PAKE message length");
        let key = self
            .state
            .finish(peer)
            .map_err(|_| anyhow::anyhow!("PAKE failed"))?;
        Ok(Unconfirmed {
            key: hmac::Key::new(hmac::HMAC_SHA256, &expand(&key, &self.context)?),
            role: self.role,
            context: self.context,
        })
    }
}

pub struct Unconfirmed {
    key: hmac::Key,
    role: Role,
    context: Vec<u8>,
}

impl Unconfirmed {
    fn input(&self, role: Role) -> Vec<u8> {
        let mut input = self.context.clone();
        input.extend(match role {
            Role::Hub => b"confirm-hub".as_slice(),
            Role::Runner => b"confirm-runner",
        });
        input
    }

    pub fn confirmation(&self) -> Vec<u8> {
        hmac::sign(&self.key, &self.input(self.role))
            .as_ref()
            .to_vec()
    }

    /// No trusted identity is returned until the other role's confirmation verifies.
    pub fn confirm(self, peer: &[u8]) -> Result<Confirmed> {
        let role = match self.role {
            Role::Hub => Role::Runner,
            Role::Runner => Role::Hub,
        };
        hmac::verify(&self.key, &self.input(role), peer)
            .map_err(|_| anyhow::anyhow!("pairing confirmation failed"))?;
        Ok(Confirmed(self.context))
    }
}

pub struct Confirmed(Vec<u8>);

/// In-memory code retirement fixture. Durable two-peer trust commits are a later gate.
pub struct Window {
    context: Vec<u8>,
    token: [u8; 16],
    deadline: Instant,
    failures: u8,
    consumed: bool,
}

impl Window {
    pub fn new(derived: &Derived, context: &Context, token: [u8; 16], deadline: Instant) -> Self {
        Self {
            context: context.encode(derived),
            token,
            deadline,
            failures: 0,
            consumed: false,
        }
    }

    pub fn admit(&self, token: &[u8; 16], now: Instant) -> Result<()> {
        ensure!(
            !self.consumed && now < self.deadline && self.failures < 3,
            "pairing session closed"
        );
        // Constant-time comparison through ring's HMAC verification primitive.
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.token);
        let offered = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, token), b"admission");
        hmac::verify(&key, b"admission", offered.as_ref())
            .map_err(|_| anyhow::anyhow!("pairing not admitted"))?;
        Ok(())
    }

    pub fn failed(&mut self, token: &[u8; 16], now: Instant) -> Result<()> {
        self.admit(token, now)?;
        self.failures += 1;
        Ok(())
    }

    pub fn consume(&mut self, token: &[u8; 16], now: Instant, proof: Confirmed) -> Result<()> {
        self.admit(token, now)?;
        ensure!(
            proof.0 == self.context,
            "confirmation belongs to another pairing"
        );
        self.consumed = true;
        Ok(())
    }
}
