//! The runner protocol fixtures (`tests/fixtures/runner-protocol-<version>`, captured from a
//! real revision of each version by `docs/tools/capture-runner-fixtures.sh`) against this
//! build: the evidence behind [`DRAINABLE_PROTOCOLS`] (see there for what a drainable version
//! must show), and a guard on the current version, whose fixtures are what its runners send.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;
use twr_core::event::{EventBody, KNOWN_KINDS};
use twr_core::runner::{
    auth_proof, check_auth_proof, drainable, HubMsg, Prover, RunnerMsg, Secret,
    DRAINABLE_PROTOCOLS, PROTOCOL_VERSION, SEALED_AUTH_PROOF,
};
use twr_core::testkit::outbound_samples;

const FILES: [&str; 3] = ["runner.json", "hub-accepts.json", "auth-vectors.json"];

fn dir(protocol: u32) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("runner-protocol-{protocol}"))
}

/// A fixture file of `protocol`, checked to be of that protocol and to name the commit it was
/// captured from (with `"uncommitted": true` when changes on top of it were captured too:
/// `capture-runner-fixtures.sh --worktree`).
fn fixture(protocol: u32, file: &str) -> Value {
    let path = dir(protocol).join(file);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let v: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display()));
    assert_eq!(v["protocol"], protocol, "{}", path.display());
    assert!(
        v["revision"].as_str().is_some_and(|r| r.len() == 40),
        "{} names no revision it was captured from",
        path.display()
    );
    assert!(
        v.get("uncommitted").is_none_or(Value::is_boolean),
        "{}: uncommitted is not a boolean",
        path.display()
    );
    v
}

/// The versions whose fixtures this build must pass: each drainable one, and the current one.
fn checked() -> Vec<u32> {
    let mut v = DRAINABLE_PROTOCOLS.to_vec();
    v.push(PROTOCOL_VERSION);
    v
}

/// `frame` with a `cmd_result`'s `"result": null` taken out: a runner sends it for an op that
/// answers nothing, and it decodes as no result (the same outcome), so a round trip drops it.
fn without_null_result(frame: &Value) -> Value {
    let mut f = frame.clone();
    if f["type"] == "cmd_result" && f.get("result").is_some_and(Value::is_null) {
        f.as_object_mut().unwrap().remove("result");
    }
    f
}

/// Only older versions are drainable, each at most once, and each checked version has its
/// three fixture files.
#[test]
fn drainable_protocols_are_older_and_have_their_fixtures() {
    assert!(!drainable(PROTOCOL_VERSION));
    let mut seen = BTreeSet::new();
    for p in DRAINABLE_PROTOCOLS {
        assert!(
            *p < PROTOCOL_VERSION,
            "{p} is not older than {PROTOCOL_VERSION}"
        );
        assert!(seen.insert(*p), "{p} is listed twice");
    }
    for p in checked() {
        for f in FILES {
            fixture(p, f);
        }
    }
}

/// Every frame a runner of a checked version sends decodes as this build's [`RunnerMsg`], each
/// event to the kind it had there (never [`EventBody::Unknown`]), and every kind such a runner
/// emits has a sample. For the current version the frames also encode back as they are: a
/// change to a message a runner sends fails here until its fixtures are captured again (or the
/// version is bumped).
#[test]
fn runner_frames_of_checked_protocols_decode_here() {
    for p in checked() {
        let f = fixture(p, "runner.json");
        let mut kinds = BTreeSet::new();
        let mut types = BTreeSet::new();
        for s in f["samples"].as_array().expect("samples") {
            let name = s["name"].as_str().unwrap();
            let frame = &s["frame"];
            let m: RunnerMsg = serde_json::from_value(frame.clone()).unwrap_or_else(|e| {
                panic!("protocol {p}: runner frame {name:?} does not decode here: {e}")
            });
            types.insert(frame["type"].as_str().unwrap().to_string());
            match &m {
                RunnerMsg::Hello(h) => assert_eq!(h.protocol, p, "{name:?}"),
                RunnerMsg::Event { body, .. } => {
                    assert!(
                        !matches!(body, EventBody::Unknown { .. }),
                        "protocol {p}: {name:?} is of a kind this build does not know"
                    );
                    assert_eq!(body.kind(), s["event_kind"], "protocol {p}: {name:?}");
                    kinds.insert(body.kind().to_string());
                }
                _ => {}
            }
            if p == PROTOCOL_VERSION {
                let back = serde_json::to_value(&m).unwrap();
                assert_eq!(
                    without_null_result(&back),
                    without_null_result(frame),
                    "protocol {p}: this build encodes {name:?} differently from its fixture: \
                     capture the fixtures again (docs/tools/capture-runner-fixtures.sh), or bump \
                     PROTOCOL_VERSION if older runners can't read it"
                );
            }
        }
        for t in [
            "hello",
            "auth",
            "inventory",
            "event",
            "exited",
            "synced",
            "cmd_result",
            "ping",
            "pong",
        ] {
            assert!(types.contains(t), "protocol {p}: no {t} sample");
        }
        for k in f["emitted_kinds"].as_array().expect("emitted_kinds") {
            let k = k.as_str().unwrap();
            assert!(
                KNOWN_KINDS.contains(&k),
                "protocol {p}: {k} is unknown here"
            );
            assert!(kinds.contains(k), "protocol {p}: no sample of {k}");
        }
    }
}

/// What this hub sends a drained runner is what each checked version decoded and encoded back
/// unchanged when its fixtures were captured: any change to one of these values fails here
/// until it is captured again against that version's code. Every command among them is
/// drain-safe, and each drain-safe op is among them.
#[test]
fn outbound_values_are_what_checked_protocols_accepted() {
    let ours: Vec<(String, Value)> = outbound_samples()
        .into_iter()
        .map(|(n, m)| (n.to_string(), serde_json::to_value(m).unwrap()))
        .collect();
    let mut ops = BTreeSet::new();
    for (name, m) in outbound_samples() {
        if let HubMsg::Cmd { op, .. } = &m {
            assert!(op.drain_safe(), "{name:?}: {} is not drain-safe", op.name());
            ops.insert(op.name());
        }
    }
    assert_eq!(ops, BTreeSet::from(["respond", "send", "stop"]));
    for p in checked() {
        let f = fixture(p, "hub-accepts.json");
        let accepted: Vec<(String, Value)> = f["accepted"]
            .as_array()
            .expect("accepted")
            .iter()
            .map(|a| (a["name"].as_str().unwrap().to_string(), a["frame"].clone()))
            .collect();
        for (name, frame) in &accepted {
            let m: HubMsg = serde_json::from_value(frame.clone()).unwrap_or_else(|e| {
                panic!("protocol {p}: accepted value {name:?} does not decode here: {e}")
            });
            assert_eq!(&serde_json::to_value(m).unwrap(), frame, "{name:?}");
        }
        let names = |v: &[(String, Value)]| v.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(&ours),
            names(&accepted),
            "protocol {p}: the outbound samples changed since its fixtures were captured: \
             capture them again (docs/tools/capture-runner-fixtures.sh)"
        );
        for ((name, v), (_, a)) in ours.iter().zip(&accepted) {
            assert_eq!(
                v, a,
                "protocol {p}: this hub now sends {name:?} differently from what that \
                 protocol's runner was shown to accept: capture again \
                 (docs/tools/capture-runner-fixtures.sh), and drop the protocol from \
                 DRAINABLE_PROTOCOLS if it no longer passes"
            );
        }
    }
}

/// This build's handshake proofs are the ones each checked version computes, for both sides,
/// and the sealed channel and its runner proof are the same.
#[test]
fn handshake_proofs_match_the_pinned_vectors() {
    for p in checked() {
        let f = fixture(p, "auth-vectors.json");
        let mut tokens = 0;
        for v in f["vectors"].as_array().expect("vectors") {
            let s = |k: &str| v[k].as_str().unwrap_or_else(|| panic!("{v}: no {k}"));
            match (s("connection"), s("by")) {
                ("token", by) => {
                    let by = match by {
                        "hub" => Prover::Hub,
                        "runner" => Prover::Runner,
                        other => panic!("{other}"),
                    };
                    let t = Secret::new(s("token"));
                    let (b, rn, hn) = (s("box"), s("runner_nonce"), s("hub_nonce"));
                    assert_eq!(
                        auth_proof(&t, by, b, rn, hn),
                        s("proof"),
                        "protocol {p}: {v}"
                    );
                    assert!(check_auth_proof(&t, by, b, rn, hn, s("proof")));
                    tokens += 1;
                }
                ("sealed", "runner") => {
                    assert_eq!(SEALED_AUTH_PROOF, s("proof"), "protocol {p}");
                }
                other => panic!("protocol {p}: unknown vector {other:?}"),
            }
        }
        assert!(tokens >= 2, "protocol {p}: no token vectors");
        let sealed = &f["sealed"];
        assert_eq!(
            sealed["channel_version"],
            twr_core::sealed::CHANNEL_VERSION,
            "protocol {p}"
        );
        assert_eq!(sealed["hello_prefix"], twr_core::sealed::HELLO_PREFIX);
        assert_eq!(sealed["key_prefix"], twr_core::sealed::KEY_PREFIX);
    }
}
