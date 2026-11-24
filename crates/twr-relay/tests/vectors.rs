//! The version 1 test vectors (docs/relay-protocol.md §13): `tests/vectors/v1.json`.
//!
//! `cargo test -p twr-relay --test vectors` checks that this build computes exactly the file's
//! values, and that every signature in it verifies. Once version 1 is released the file is
//! frozen: a change to it is a protocol change. Before that, regenerate it with
//! `TWR_RELAY_WRITE_VECTORS=1 cargo test -p twr-relay --test vectors`.

use serde_json::{json, Value};
use twr_relay::{admit, client, keys, names, proto, IdentityKey};

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/v1.json");

fn seed(n: u8) -> [u8; 32] {
    [n; 32]
}

fn hex32(b: &[u8; 32]) -> String {
    hex::encode(b)
}

/// Every value derived deterministically. P-256 signatures are randomized, so the file holds
/// one valid signature per P-256 case, checked by verifying rather than by comparing.
fn generate(p256_sigs: Option<&Value>) -> Value {
    // Hub names from pins.
    let ed = IdentityKey::ed25519_from_seed(&seed(1)).unwrap();
    let pins: Vec<[u8; 32]> = vec![[0u8; 32], [0xffu8; 32], ed.pin()];
    let hub_names: Vec<Value> = pins
        .iter()
        .map(|p| json!({ "pin": names::pin_text(p), "hub_name": names::hub_name(p) }))
        .collect();

    // Tags, labels and server names.
    let hub = names::hub_name(&ed.pin());
    let tags: Vec<Value> = [
        (seed(2), 0u64),
        (seed(2), 6_000_000),
        (seed(3), 6_000_001),
        ([0u8; 32], u64::MAX / 300),
    ]
    .iter()
    .map(|(key, window)| {
        let tag = names::tag(key, &hub, *window);
        json!({
            "relay_key": names::b64(key),
            "hub_name": hub,
            "window": window,
            "unix_seconds_example": window.saturating_mul(300),
            "tag": tag,
            "label": names::label(&hub, &tag),
            "sni": names::phone_sni(&hub, &tag, "eu.relay.twr.tools"),
        })
    })
    .collect();

    // Hub authentication.
    let exporter = [0x5au8; 32];
    let msg = proto::hub_auth_message(&exporter);
    let ed_auth = json!({
        "key": "ed25519",
        "seed": hex32(&seed(1)),
        "spki": names::b64(&ed.spki()),
        "pin": names::pin_text(&ed.pin()),
        "hub_name": hub,
        "exporter": hex32(&exporter),
        "signed": hex::encode(&msg),
        "sig": names::b64(&ed.sign(&msg).unwrap()),
    });
    // A fixed P-256 key (PKCS#8 generated once and kept in the file).
    let p256_pkcs8 = p256_sigs
        .and_then(|v| v["hub_auth"][1]["pkcs8"].as_str())
        .and_then(names::unb64)
        .unwrap_or_else(|| {
            IdentityKey::generate_p256()
                .unwrap()
                .pkcs8()
                .unwrap()
                .to_vec()
        });
    let p256 = IdentityKey::from_pkcs8(&p256_pkcs8).unwrap();
    let p256_sig = p256_sigs
        .and_then(|v| v["hub_auth"][1]["sig"].as_str().map(String::from))
        .unwrap_or_else(|| names::b64(&p256.sign(&msg).unwrap()));
    let p256_auth = json!({
        "key": "p256",
        "pkcs8": names::b64(&p256_pkcs8),
        "spki": names::b64(&p256.spki()),
        "pin": names::pin_text(&p256.pin()),
        "hub_name": names::hub_name(&p256.pin()),
        "exporter": hex32(&exporter),
        "signed": hex::encode(&msg),
        "sig": p256_sig,
        "sig_note": "ECDSA signatures are randomized: verify this one, don't expect to reproduce it",
    });

    // A moved note.
    let routes = json!(["relay://us.relay.twr.tools", "203.0.113.5:47321"]);
    let (body, sig) = client::sign_moved(&ed, 1_800_000_000, routes.clone()).unwrap();
    let moved = json!({
        "seed": hex32(&seed(1)),
        "spki": names::b64(&ed.spki()),
        "issued": 1_800_000_000u64,
        "routes": routes,
        "body": body,
        "body_text": String::from_utf8(names::unb64(&body).unwrap()).unwrap(),
        "sig": sig,
    });

    // Admissions.
    let a = IdentityKey::ed25519_from_seed(&seed(4)).unwrap();
    let b = IdentityKey::ed25519_from_seed(&seed(5)).unwrap();
    let ea: [u8; 32] = a.public_raw().try_into().unwrap();
    let eb: [u8; 32] = b.public_raw().try_into().unwrap();
    let pairing = admit::sign_pairing(&a, 1_800_000_000).unwrap();
    let exp = 1_802_592_000u64;
    let pair = admit::pair_admission(
        &ea,
        &eb,
        exp,
        admit::sign_pair_half(&a, &ea, &eb, exp).unwrap(),
        admit::sign_pair_half(&b, &ea, &eb, exp).unwrap(),
    );
    let admissions = json!({
        "pairing": {
            "seed": hex32(&seed(4)),
            "signed": hex::encode(admit::pairing_message(&ea, 1_800_000_000)),
            "json": serde_json::to_value(&pairing).unwrap(),
        },
        "pair": {
            "seed_a": hex32(&seed(4)),
            "seed_b": hex32(&seed(5)),
            "signed": hex::encode(admit::pair_message(&ea, &eb, exp)),
            "json": serde_json::to_value(&pair).unwrap(),
        },
    });

    // A frame and a record signable.
    let ping = proto::encode(&proto::Msg::Ping { n: 1 });
    let mut frame = (ping.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&ping);

    json!({
        "version": 1,
        "about": "Test vectors for docs/relay-protocol.md version 1. Byte strings are hex unless the field is base64url (b64 in the spec: pins, keys, spki, sig, body). Frozen once version 1 is released.",
        "hub_names": hub_names,
        "tags": tags,
        "hub_auth": [ed_auth, p256_auth],
        "moved": moved,
        "admissions": admissions,
        "frames": [{ "message": serde_json::from_slice::<Value>(&ping).unwrap(), "bytes": hex::encode(&frame) }],
        "record_signable": {
            "timestamp_us": 1_800_000_000_000_000u64,
            "packet": hex::encode([0u8; 12]),
            "signable_text": String::from_utf8_lossy(&signable(1_800_000_000_000_000, &[0u8; 12])).replace('\0', "\\0"),
            "signable": hex::encode(signable(1_800_000_000_000_000, &[0u8; 12])),
        },
    })
}

/// pkarr's BEP 44 signable (§9), written out independently of the server's copy.
fn signable(ts: u64, packet: &[u8]) -> Vec<u8> {
    let mut s = format!("3:seqi{ts}e1:v{}:", packet.len()).into_bytes();
    s.extend_from_slice(packet);
    s
}

#[test]
fn vectors_v1() {
    let existing: Option<Value> = std::fs::read(PATH)
        .ok()
        .map(|b| serde_json::from_slice(&b).unwrap());
    let generated = generate(existing.as_ref());
    if std::env::var_os("TWR_RELAY_WRITE_VECTORS").is_some() {
        std::fs::write(
            PATH,
            serde_json::to_string_pretty(&generated).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let existing = existing.expect("tests/vectors/v1.json exists");
    assert_eq!(
        generated, existing,
        "this build disagrees with the frozen vectors"
    );
}

#[test]
fn every_signature_verifies() {
    let v: Value = serde_json::from_slice(&std::fs::read(PATH).unwrap()).unwrap();
    for case in v["hub_auth"].as_array().unwrap() {
        let spki = names::unb64(case["spki"].as_str().unwrap()).unwrap();
        let signed = hex::decode(case["signed"].as_str().unwrap()).unwrap();
        let sig = names::unb64(case["sig"].as_str().unwrap()).unwrap();
        assert!(keys::verify(&spki, &signed, &sig), "{case}");
        assert_eq!(
            names::pin_text(&names::pin(&spki)),
            case["pin"].as_str().unwrap()
        );
    }
    let m = &v["moved"];
    let pin = names::pin(&names::unb64(m["spki"].as_str().unwrap()).unwrap());
    assert!(client::verify_moved(
        &pin,
        m["spki"].as_str().unwrap(),
        m["body"].as_str().unwrap(),
        m["sig"].as_str().unwrap()
    )
    .is_some());
    for kind in ["pairing", "pair"] {
        let body = serde_json::to_vec(&v["admissions"][kind]["json"]).unwrap();
        // Checked at a time the records are valid for.
        assert!(admit::verify(&body, 1_800_000_100).is_ok(), "{kind}");
    }
}
