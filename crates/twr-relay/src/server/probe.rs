//! `twr relay probe`: checks a running relay end to end, as monitoring and the compatibility
//! job do (docs/relay-protocol.md §13).

use crate::client::{self, HubConfig, HubSession, Target};
use crate::{admit, names, IdentityKey};
use anyhow::{anyhow, bail, Context, Result};
use rustls::pki_types::CertificateDer;
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub ok: bool,
    pub relay: String,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone)]
pub struct ProbeOptions {
    pub target: Target,
    pub client: String,
    pub timeout: Duration,
    /// Keys kept between runs, so a probe every minute is one hub and one admitted pair to
    /// the relay, not a new one each time (which the per-address limits would refuse).
    pub state: Option<std::path::PathBuf>,
}

/// The probe's keys: kept in `state` when given, otherwise new for this run.
struct Keys {
    hub: IdentityKey,
    /// iroh endpoint seeds; with `state`, two that post a pair admission.
    a: [u8; 32],
    b: Option<[u8; 32]>,
}

fn load_or_make(
    dir: &std::path::Path,
    name: &str,
    make: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let path = dir.join(name);
    match std::fs::read(&path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let b = make()?;
            super::certs::write_private(&path, &b)?;
            Ok(b)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn keys(state: Option<&std::path::Path>) -> Result<Keys> {
    let Some(dir) = state else {
        return Ok(Keys {
            hub: IdentityKey::generate_p256()?,
            a: rand_bytes(),
            b: None,
        });
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let hub = load_or_make(dir, "probe-hub.pk8", || {
        Ok(IdentityKey::generate_p256()?.pkcs8().unwrap().to_vec())
    })?;
    let a = load_or_make(dir, "probe-iroh-a.seed", || Ok(rand_bytes::<32>().to_vec()))?;
    let b = load_or_make(dir, "probe-iroh-b.seed", || Ok(rand_bytes::<32>().to_vec()))?;
    Ok(Keys {
        hub: IdentityKey::from_pkcs8(&hub)?,
        a: a.try_into().map_err(|_| anyhow!("bad probe-iroh-a.seed"))?,
        b: Some(b.try_into().map_err(|_| anyhow!("bad probe-iroh-b.seed"))?),
    })
}

/// A TLS server configuration presenting `key`, as the phone gateway does: a self-signed
/// certificate the phone checks by pin only.
pub fn identity_tls(key: &IdentityKey, alpn: &[&[u8]]) -> Result<rustls::ServerConfig> {
    let der = key.pkcs8().context("the key has no PKCS#8 form")?;
    let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(der.to_vec());
    let pair = rcgen::KeyPair::try_from(&pkcs8).map_err(|e| anyhow!("{e}"))?;
    let params = rcgen::CertificateParams::new(vec!["twr-hub.invalid".to_string()])?;
    let cert = params.self_signed(&pair)?;
    let mut cfg = rustls::ServerConfig::builder_with_provider(crate::tls::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(pkcs8),
        )?;
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    Ok(cfg)
}

async fn timed<F, T>(
    checks: &mut Vec<Check>,
    name: &'static str,
    limit: Duration,
    f: F,
) -> Option<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let start = Instant::now();
    let r = match tokio::time::timeout(limit, f).await {
        Ok(r) => r,
        Err(_) => Err(anyhow!("timed out after {} s", limit.as_secs())),
    };
    let ms = start.elapsed().as_millis() as u64;
    match r {
        Ok(v) => {
            checks.push(Check {
                name,
                ok: true,
                ms,
                error: None,
            });
            Some(v)
        }
        Err(e) => {
            checks.push(Check {
                name,
                ok: false,
                ms,
                error: Some(format!("{e:#}")),
            });
            None
        }
    }
}

pub async fn probe(o: &ProbeOptions) -> Report {
    let mut checks = Vec::new();
    let t = &o.target;
    let info = timed(&mut checks, "info", o.timeout, async {
        let v = client::info(t).await?;
        if v.get("relay").and_then(|r| r.as_str()) != Some("twr-relay") {
            bail!("not a twr relay");
        }
        Ok(v)
    })
    .await;

    let keys = match keys(o.state.as_deref()) {
        Ok(k) => k,
        Err(e) => {
            checks.push(Check {
                name: "probe_state",
                ok: false,
                ms: 0,
                error: Some(format!("{e:#}")),
            });
            return Report {
                ok: false,
                relay: t.url.to_string(),
                checks,
            };
        }
    };
    // A test hub with one phone, and a round trip through the relay.
    let hub_key = Arc::new(keys.hub);
    let pin = hub_key.pin();
    let cfg = HubConfig::new(t.clone(), o.client.clone());
    let session = timed(
        &mut checks,
        "hub_session",
        o.timeout,
        HubSession::connect(&cfg, &hub_key),
    )
    .await;
    if let Some(mut session) = session {
        let (cred, relay_key) = client::new_phone_credential().expect("randomness");
        let set = timed(
            &mut checks,
            "phone_credentials",
            o.timeout,
            session.set_phones(vec![cred]),
        )
        .await;
        if set.is_some() {
            if let Some(route) = session.welcome.phone_routes.first().cloned() {
                let tls = identity_tls(&hub_key, &[b"twr-probe"]).expect("a TLS config");
                timed(&mut checks, "phone_round_trip", o.timeout, async {
                    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
                    let hub_side = async {
                        let s = session
                            .accept()
                            .await
                            .context("the relay passed no phone stream")?;
                        let mut t = acceptor.accept(s.io).await.context("hub-side TLS")?;
                        let mut buf = [0u8; 32];
                        t.read_exact(&mut buf).await?;
                        t.write_all(&buf).await?;
                        t.flush().await?;
                        // Wait for the phone to finish reading before closing.
                        let _ = t.read(&mut [0u8; 1]).await;
                        Ok::<_, anyhow::Error>(())
                    };
                    let phone_side = async {
                        let mut t =
                            client::phone_connect(&route, &pin, &relay_key, &[b"twr-probe"])
                                .await?;
                        let nonce: [u8; 32] = rand_bytes();
                        t.write_all(&nonce).await?;
                        t.flush().await?;
                        let mut back = [0u8; 32];
                        t.read_exact(&mut back).await?;
                        if back != nonce {
                            bail!("the echo differs");
                        }
                        t.shutdown().await.ok();
                        Ok(())
                    };
                    let (a, b) = tokio::join!(hub_side, phone_side);
                    a.and(b)
                })
                .await;
                timed(&mut checks, "status_page", o.timeout, async {
                    match client::hub_status(t, &names::hub_name(&pin), &relay_key).await? {
                        Some(true) => Ok(()),
                        other => bail!("status page says {other:?}"),
                    }
                })
                .await;
            } else {
                checks.push(Check {
                    name: "phone_round_trip",
                    ok: false,
                    ms: 0,
                    error: Some("no phone route".into()),
                });
            }
        }
        session.close();
    }

    // The record store.
    timed(&mut checks, "records", o.timeout, async {
        let key = IdentityKey::generate_ed25519()?;
        let pk: [u8; 32] = key.public_raw().try_into()?;
        let ts = crate::now_unix() * 1_000_000;
        // An empty DNS message: header only.
        let packet = [0u8; 12];
        let sig = key.sign(&super::stores::signable(ts, &packet))?;
        let body = [sig.as_slice(), &ts.to_be_bytes(), &packet].concat();
        let path = format!("/pkarr/{}", crate::b32::zencode(&pk));
        let (st, _) = client::https(t, "PUT", &path, Some(body.clone())).await?;
        if st != 200 {
            bail!("PUT answered {st}");
        }
        let (st, got) = client::https(t, "GET", &path, None).await?;
        if st != 200 || got != body {
            bail!("GET answered {st} with {} bytes", got.len());
        }
        Ok(())
    })
    .await;

    // iroh: admit a fresh endpoint, then complete a relay handshake with it.
    let iroh_url = info
        .as_ref()
        .and_then(|v| v.get("iroh_url"))
        .and_then(|u| u.as_str())
        .map(String::from);
    let seed = keys.a;
    let admitted = timed(&mut checks, "iroh_admission", o.timeout, async {
        let key = IdentityKey::ed25519_from_seed(&seed)?;
        let a = match keys.b {
            None => admit::sign_pairing(&key, crate::now_unix())?,
            Some(b) => {
                let other = IdentityKey::ed25519_from_seed(&b)?;
                let (ea, eb): ([u8; 32], [u8; 32]) =
                    (key.public_raw().try_into()?, other.public_raw().try_into()?);
                let exp = crate::now_unix() + 2 * 86400;
                admit::pair_admission(
                    &ea,
                    &eb,
                    exp,
                    admit::sign_pair_half(&key, &ea, &eb, exp)?,
                    admit::sign_pair_half(&other, &ea, &eb, exp)?,
                )
            }
        };
        client::post_admission(t, &a).await
    })
    .await;
    if let (Some(url), Some(_)) = (iroh_url, admitted) {
        timed(
            &mut checks,
            "iroh_relay",
            o.timeout,
            iroh_handshake(url, seed, t.url.pin, t.extra_roots.clone()),
        )
        .await;
    }

    let ok = checks.iter().all(|c| c.ok);
    Report {
        ok,
        relay: t.url.to_string(),
        checks,
    }
}

async fn iroh_handshake(
    url: String,
    seed: [u8; 32],
    pin: Option<[u8; 32]>,
    roots: Vec<CertificateDer<'static>>,
) -> Result<()> {
    let relay_url: iroh_base::RelayUrl = url.parse().context("the relay's iroh_url")?;
    let secret = iroh_base::SecretKey::from_bytes(&seed);
    let tls = if pin.is_none() && !roots.is_empty() {
        crate::tls::client_config_with_roots(roots, &[])
    } else {
        crate::tls::client_config(pin, &[])
    };
    // iroh's relay client speaks TLS 1.2 or 1.3; our helper allows 1.3, which the relay serves.
    let client = iroh_relay::client::ClientBuilder::new(
        relay_url,
        secret,
        iroh_dns::dns::DnsResolver::new(),
    )
    .tls_client_config(tls)
    .connect()
    .await
    .map_err(|e| anyhow!("{e}"))?;
    drop(client);
    Ok(())
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut b = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .expect("randomness");
    b
}

/// Prints a report and returns the exit status.
pub fn print(report: &Report, json: bool) -> i32 {
    if json {
        println!("{}", serde_json::to_string(report).unwrap());
    } else {
        for c in &report.checks {
            match &c.error {
                None => println!("ok    {:<18} {:>6} ms", c.name, c.ms),
                Some(e) => println!("FAIL  {:<18} {:>6} ms  {e}", c.name, c.ms),
            }
        }
        println!(
            "{}: {}",
            report.relay,
            if report.ok {
                "all checks passed"
            } else {
                "FAILED"
            }
        );
    }
    if report.ok {
        0
    } else {
        1
    }
}
