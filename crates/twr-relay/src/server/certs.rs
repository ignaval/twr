//! The relay's own certificate: Let's Encrypt (tls-alpn-01 on the main port), a self-signed
//! key that hubs pin, or PEM files (docs/relay.md). Only the relay's own names use it; phone
//! traffic is never decrypted.

use super::config::{Config, TlsMode};
use super::counters::Counters;
use crate::{der, names, proto};
use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt as _;
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

/// File names in the state directory.
pub const SELF_KEY: &str = "relay-key.pem";
pub const ACME_DIR: &str = "acme";

#[derive(Debug)]
enum Inner {
    Acme(Arc<tokio_rustls_acme::ResolvesServerCertAcme>),
    Static(RwLock<Arc<CertifiedKey>>),
}

/// Picks the relay's certificate for its own names.
#[derive(Debug)]
pub struct Resolver(Inner);

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        match &self.0 {
            Inner::Acme(r) => r.resolve(hello),
            Inner::Static(k) => Some(k.read().unwrap().clone()),
        }
    }
}

pub struct Certs {
    pub resolver: Arc<Resolver>,
    /// The relay's pin when its certificate is self-signed (§5.4 `relay_pin`).
    pub self_pin: Option<[u8; 32]>,
}

fn certified(chain: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> Result<Arc<CertifiedKey>> {
    let provider = rustls::crypto::ring::default_provider();
    let signer = provider.key_provider.load_private_key(key).map_err(|e| anyhow!("loading the key: {e}"))?;
    Ok(Arc::new(CertifiedKey::new(chain, signer)))
}

fn note_expiry(counters: &Counters, cert: &[u8]) {
    if let Some(t) = der::cert_not_after(cert) {
        counters.cert_not_after.store(t, Relaxed);
    }
}

/// Writes `data` to `path` with mode 0600, atomically.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The self-signed key, made on first use, and its pin.
pub fn self_signed_key(state_dir: &Path) -> Result<(rcgen::KeyPair, [u8; 32])> {
    let path = state_dir.join(SELF_KEY);
    let pair = match std::fs::read_to_string(&path) {
        Ok(pem) => rcgen::KeyPair::from_pem(&pem).context("reading the relay's key")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
            std::fs::create_dir_all(state_dir)?;
            write_private(&path, pair.serialize_pem().as_bytes())?;
            pair
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let pin = names::pin(&rcgen::PublicKeyData::subject_public_key_info(&pair));
    Ok((pair, pin))
}

fn self_signed(cfg: &Config) -> Result<(Arc<CertifiedKey>, [u8; 32])> {
    let (pair, pin) = self_signed_key(&cfg.relay.state_dir)?;
    let mut sans: Vec<String> = Vec::new();
    sans.extend(cfg.relay.hostname.clone());
    sans.extend(cfg.iroh_hostname());
    if let Some(h) = &cfg.relay.public_host {
        if !sans.contains(h) {
            sans.push(h.clone());
        }
    }
    if sans.is_empty() {
        sans.push("relay.invalid".into());
    }
    let mut params = rcgen::CertificateParams::new(sans)?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "twr relay");
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    let cert = params.self_signed(&pair)?;
    let key = PrivateKeyDer::try_from(pair.serialize_der()).map_err(|e| anyhow!("{e}"))?;
    Ok((certified(vec![cert.der().clone()], key)?, pin))
}

fn load_files(cert: &Path, key: &Path) -> Result<Arc<CertifiedKey>> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("reading {}: {e}", cert.display()))?;
    if chain.is_empty() {
        bail!("{} holds no certificate", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| anyhow!("reading {}: {e}", key.display()))?;
    certified(chain, key)
}

/// A cache for the ACME crate that also reports the stored certificate's expiry.
struct Cache {
    dir: tokio_rustls_acme::caches::DirCache<PathBuf>,
    counters: Arc<Counters>,
}

fn first_cert(pem: &[u8]) -> Option<Vec<u8>> {
    CertificateDer::pem_slice_iter(pem).next()?.ok().map(|c| c.to_vec())
}

#[async_trait::async_trait]
impl tokio_rustls_acme::CertCache for Cache {
    type EC = std::io::Error;
    async fn load_cert(&self, domains: &[String], directory_url: &str) -> Result<Option<Vec<u8>>, Self::EC> {
        let r = self.dir.load_cert(domains, directory_url).await?;
        if let Some(c) = r.as_deref().and_then(first_cert) {
            note_expiry(&self.counters, &c);
        }
        Ok(r)
    }
    async fn store_cert(&self, domains: &[String], directory_url: &str, cert: &[u8]) -> Result<(), Self::EC> {
        if let Some(c) = first_cert(cert) {
            note_expiry(&self.counters, &c);
        }
        self.dir.store_cert(domains, directory_url, cert).await
    }
}

#[async_trait::async_trait]
impl tokio_rustls_acme::AccountCache for Cache {
    type EA = std::io::Error;
    async fn load_account(&self, contact: &[String], directory_url: &str) -> Result<Option<Vec<u8>>, Self::EA> {
        self.dir.load_account(contact, directory_url).await
    }
    async fn store_account(&self, contact: &[String], directory_url: &str, account: &[u8]) -> Result<(), Self::EA> {
        self.dir.store_account(contact, directory_url, account).await
    }
}

impl Certs {
    pub fn start(cfg: &Config, counters: Arc<Counters>, cancel: CancellationToken) -> Result<Self> {
        match cfg.tls.mode {
            TlsMode::SelfSigned => {
                let (key, pin) = self_signed(cfg)?;
                note_expiry(&counters, key.cert[0].as_ref());
                Ok(Self { resolver: Arc::new(Resolver(Inner::Static(RwLock::new(key)))), self_pin: Some(pin) })
            }
            TlsMode::Files => {
                let (cp, kp) = (cfg.tls.cert.clone().unwrap(), cfg.tls.key.clone().unwrap());
                let key = load_files(&cp, &kp)?;
                note_expiry(&counters, key.cert[0].as_ref());
                let resolver = Arc::new(Resolver(Inner::Static(RwLock::new(key))));
                // Pick up renewed files every hour.
                let r = resolver.clone();
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
                    tick.tick().await;
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tick.tick() => {}
                        }
                        match load_files(&cp, &kp) {
                            Ok(k) => {
                                note_expiry(&counters, k.cert[0].as_ref());
                                if let Inner::Static(lock) = &r.0 {
                                    *lock.write().unwrap() = k;
                                }
                            }
                            Err(e) => tracing::warn!("reloading the certificate files failed: {e:#}"),
                        }
                    }
                });
                Ok(Self { resolver, self_pin: None })
            }
            TlsMode::Acme => {
                let mut domains: Vec<String> = cfg.relay.hostname.iter().cloned().collect();
                domains.extend(cfg.iroh_hostname());
                let client_tls = match &cfg.tls.acme_ca_roots {
                    Some(path) => {
                        let roots: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
                            .with_context(|| format!("reading {}", path.display()))?
                            .collect::<Result<_, _>>()
                            .map_err(|e| anyhow!("{e}"))?;
                        let mut store = rustls::RootCertStore::empty();
                        for r in roots {
                            store.add(r)?;
                        }
                        rustls::ClientConfig::builder_with_provider(crate::tls::provider())
                            .with_safe_default_protocol_versions()?
                            .with_root_certificates(store)
                            .with_no_client_auth()
                    }
                    None => rustls::ClientConfig::builder_with_provider(crate::tls::provider())
                        .with_safe_default_protocol_versions()?
                        .with_root_certificates(rustls::RootCertStore {
                            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                        })
                        .with_no_client_auth(),
                };
                let dir = cfg.relay.state_dir.join(ACME_DIR);
                std::fs::create_dir_all(&dir)?;
                let mut acme = tokio_rustls_acme::AcmeConfig::new_with_client_tls_config(domains, Arc::new(client_tls))
                    .directory(&cfg.tls.acme_directory)
                    .cache(Cache { dir: tokio_rustls_acme::caches::DirCache::new(dir), counters });
                if let Some(c) = &cfg.tls.acme_contact {
                    acme = acme.contact_push(c);
                }
                let mut state = acme.state();
                let resolver = state.resolver();
                tokio::spawn(async move {
                    loop {
                        let ev = tokio::select! {
                            _ = cancel.cancelled() => return,
                            ev = state.next() => ev,
                        };
                        match ev {
                            Some(Ok(ok)) => tracing::info!("certificate: {ok:?}"),
                            Some(Err(err)) => tracing::warn!("certificate: {err}"),
                            None => return,
                        }
                    }
                });
                Ok(Self { resolver: Arc::new(Resolver(Inner::Acme(resolver))), self_pin: None })
            }
        }
    }

    /// The configuration for the relay's own TLS: hub sessions, the API, ACME challenges.
    pub fn own_config(&self) -> Arc<rustls::ServerConfig> {
        let mut c = rustls::ServerConfig::builder_with_provider(crate::tls::provider())
            .with_safe_default_protocol_versions()
            .expect("ring supports the defaults")
            .with_no_client_auth()
            .with_cert_resolver(self.resolver.clone());
        c.alpn_protocols = proto::HUB_ALPNS.iter().map(|a| a.to_vec()).collect();
        c.alpn_protocols.extend([b"h2".to_vec(), b"http/1.1".to_vec(), proto::ACME_ALPN.to_vec()]);
        Arc::new(c)
    }

    /// The configuration iroh's relay server terminates TLS with.
    pub fn iroh_config(&self) -> Arc<rustls::ServerConfig> {
        let mut c = rustls::ServerConfig::builder_with_provider(crate::tls::provider())
            .with_safe_default_protocol_versions()
            .expect("ring supports the defaults")
            .with_no_client_auth()
            .with_cert_resolver(self.resolver.clone());
        c.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(c)
    }
}
