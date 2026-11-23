//! Seeded fuzzing of what the relay reads from strangers before anything is authenticated:
//! the ClientHello reader (§4), the certificate walker behind pin checks, frames (§2), relay
//! URLs (§3.4), labels and admission bodies (§10.2).
//!
//! The regular run does 20k cases. The long run, before a release or after a change to any
//! of these parsers:
//!
//! ```sh
//! CARGO_TARGET_DIR=target/fuzz cargo test --release -p twr-relay --features server --test parse_fuzz -- --ignored --nocapture
//! ```
//!
//! `TWR_FUZZ_SEED` and `TWR_FUZZ_CASES` change the seed and the number of cases.

use std::sync::Arc;
use twr_relay::clienthello::{self, Parse};
use twr_relay::{admit, der, names, proto, IdentityKey, RelayUrl};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn hello(sni: Option<&str>, alpn: &[&[u8]]) -> Vec<u8> {
    let mut cfg = rustls::ClientConfig::builder_with_provider(twr_relay::tls::provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(twr_relay::tls::PinVerifier::new([0; 32])))
        .with_no_client_auth();
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    let name = match sni {
        Some(s) => rustls::pki_types::ServerName::try_from(s.to_string()).unwrap(),
        None => rustls::pki_types::ServerName::IpAddress(
            "192.0.2.1".parse::<std::net::IpAddr>().unwrap().into(),
        ),
    };
    let mut c = rustls::ClientConnection::new(Arc::new(cfg), name).unwrap();
    let mut out = Vec::new();
    c.write_tls(&mut out).unwrap();
    out
}

fn mutate(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut v = base.to_vec();
    for _ in 0..=rng.below(6) {
        match rng.below(7) {
            0 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] ^= 1 << rng.below(8);
            }
            1 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] = rng.next() as u8;
            }
            2 => {
                let i = rng.below(v.len() + 1);
                v.truncate(i);
            }
            3 => {
                let i = rng.below(v.len() + 1);
                for _ in 0..rng.below(40) {
                    v.insert(i, rng.next() as u8);
                }
            }
            4 if v.len() > 2 => {
                // A length field at a random place set to an extreme.
                let i = rng.below(v.len() - 1);
                let x = [0u8, 0xff, 0x7f, 0x80][rng.below(4)];
                v[i] = x;
                v[i + 1] = x;
            }
            5 if !v.is_empty() => {
                let i = rng.below(v.len());
                let j = rng.below(v.len());
                v.swap(i, j);
            }
            _ => {}
        }
    }
    v
}

fn run(cases: u64, seed: u64) {
    let mut rng = Rng(seed | 1);
    let long_name = [&"a".repeat(60)[..]; 4].join(".") + ".example";
    let hellos = [
        hello(Some("hub.relay.test"), &[b"h2", b"http/1.1"]),
        hello(
            Some(&format!(
                "{}-{}.eu.relay.twr.tools",
                "a".repeat(26),
                "b".repeat(26)
            )),
            &[],
        ),
        hello(None, &[proto::ALPN_V1]),
        hello(Some(&long_name), &[proto::ACME_ALPN]),
    ];
    let pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let certs: Vec<Vec<u8>> = vec![
        rcgen::CertificateParams::new(vec!["x.example".into()])
            .unwrap()
            .self_signed(&pair)
            .unwrap()
            .der()
            .to_vec(),
        rcgen::CertificateParams::new(vec!["203.0.113.9".into(), "y.example".into()])
            .unwrap()
            .self_signed(&pair)
            .unwrap()
            .der()
            .to_vec(),
    ];
    let spki = rcgen::PublicKeyData::subject_public_key_info(&pair);
    for c in &certs {
        assert_eq!(der::cert_spki(c), Some(spki.as_slice()));
        assert!(der::cert_not_after(c).is_some());
    }
    let frames: Vec<Vec<u8>> = vec![
        proto::encode(&proto::Msg::Ping { n: 3 }),
        proto::encode(&proto::Msg::Phones {
            phones: vec![proto::PhoneCred {
                id: "0".repeat(32),
                key: "k".into(),
            }],
        }),
        br#"{"type":"hello","spki":"AA","sig":"AA","client":"x","features":["a"]}"#.to_vec(),
    ];
    let a = IdentityKey::ed25519_from_seed(&[1; 32]).unwrap();
    let adm = serde_json::to_vec(&admit::sign_pairing(&a, 100).unwrap()).unwrap();
    let urls = [
        "relay://eu.relay.twr.tools",
        "relay://[2001:db8::1]:8443#AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ];

    for case in 0..cases {
        // ClientHello: every result is consistent with a re-parse, and names are normalized.
        let base = &hellos[rng.below(hellos.len())];
        let m = mutate(&mut rng, base);
        let r = clienthello::parse(&m);
        if let Parse::Done(h) = &r {
            if let Some(s) = &h.sni {
                assert!(
                    !s.is_empty() && s.len() <= 253 && !s.ends_with('.'),
                    "case {case}: {s:?}"
                );
                assert_eq!(s, &s.to_ascii_lowercase(), "case {case}");
            }
            assert!(h.alpn.iter().all(|p| !p.is_empty()), "case {case}");
        }
        // A prefix of a complete hello is never Done with a different answer.
        if let Parse::Done(h) = clienthello::parse(base) {
            let cut = rng.below(base.len());
            assert!(
                matches!(clienthello::parse(&base[..cut]), Parse::Incomplete),
                "case {case}"
            );
            let _ = h;
        }

        // DER: never panics, and a found SPKI lies inside the input.
        let i = rng.below(certs.len());
        let c = mutate(&mut rng, &certs[i]);
        if let Some(s) = der::cert_spki(&c) {
            assert!(s.len() <= c.len());
            let _ = names::pin(s);
        }
        let _ = der::cert_not_after(&c);

        // Frames and admission bodies: never panic.
        let i = rng.below(frames.len());
        let f = mutate(&mut rng, &frames[i]);
        let _ = proto::decode(&f);
        let b = mutate(&mut rng, &adm);
        let _ = admit::verify(&b, 100);

        // Text inputs: relay URLs and labels.
        let i = rng.below(urls.len());
        let u = mutate(&mut rng, urls[i].as_bytes());
        if let Ok(text) = std::str::from_utf8(&u) {
            if let Ok(url) = RelayUrl::parse(text) {
                // What parses prints back to something that parses to the same.
                assert_eq!(
                    RelayUrl::parse(&url.to_string()).unwrap(),
                    url,
                    "case {case}: {text:?}"
                );
            }
        }
        let l = mutate(
            &mut rng,
            format!("{}-{}", "a".repeat(26), "b".repeat(26)).as_bytes(),
        );
        if let Ok(text) = std::str::from_utf8(&l) {
            if let Some((h, t)) = names::parse_label(text) {
                assert!(names::is_hub_name(h) && names::is_hub_name(t));
            }
            let _ = names::split_phone_sni(text);
        }
    }
}

fn settings(default_cases: u64) -> (u64, u64) {
    let seed = std::env::var("TWR_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x7477_7272_656c_6179);
    let cases = std::env::var("TWR_FUZZ_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_cases);
    (cases, seed)
}

#[test]
fn parsers_hold_up() {
    let (cases, seed) = settings(20_000);
    run(cases, seed);
}

#[test]
#[ignore = "long run: see the module docs"]
fn parsers_hold_up_long() {
    let (cases, seed) = settings(2_000_000);
    eprintln!("parse_fuzz: {cases} cases, seed {seed}");
    run(cases, seed);
}
