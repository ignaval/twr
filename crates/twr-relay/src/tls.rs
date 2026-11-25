//! Client-side TLS: checking a relay by pin or by the WebPKI roots, and a phone's pin check.

use crate::{der, names};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    ring as provider, verify_tls12_signature, verify_tls13_signature, CryptoProvider,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use std::sync::Arc;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(provider::default_provider())
}

/// Accepts a server whose leaf certificate's SPKI pin equals the expected one, and nothing
/// else. Names, chains and dates are ignored (docs/relay-protocol.md §3.4, §6). The handshake
/// signature is still checked against that key, so only the key's holder passes.
#[derive(Debug)]
pub struct PinVerifier {
    pin: [u8; 32],
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PinVerifier {
    pub fn new(pin: [u8; 32]) -> Self {
        Self {
            pin,
            algs: provider::default_provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let spki = der::cert_spki(end_entity.as_ref()).ok_or(rustls::Error::InvalidCertificate(
            rustls::CertificateError::BadEncoding,
        ))?;
        let got = names::pin(spki);
        if names::ct_eq(&got, &self.pin) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// A TLS 1.3 client configuration that checks the server by `pin`, or by the WebPKI roots twr
/// ships when `pin` is `None`.
pub fn client_config(pin: Option<[u8; 32]>, alpn: &[&[u8]]) -> ClientConfig {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring supports TLS 1.3");
    let mut cfg = match pin {
        Some(pin) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinVerifier::new(pin)))
            .with_no_client_auth(),
        None => {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    cfg
}

/// The same with extra trusted roots, for tests and for a relay under a private CA.
pub fn client_config_with_roots(
    roots: Vec<CertificateDer<'static>>,
    alpn: &[&[u8]],
) -> ClientConfig {
    let mut store = rustls::RootCertStore::empty();
    for r in roots {
        let _ = store.add(r);
    }
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring supports TLS 1.3")
        .with_root_certificates(store)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    cfg
}
