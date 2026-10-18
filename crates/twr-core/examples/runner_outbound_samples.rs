//! Prints the values this build's hub sends a runner it drains
//! ([`twr_core::testkit::outbound_samples`]) as one JSON object, `{"samples": [{"name",
//! "frame"}]}`: what `docs/tools/capture-runner-fixtures.sh` has an older revision decode.

fn main() {
    let samples: Vec<_> = twr_core::testkit::outbound_samples()
        .into_iter()
        .map(|(name, m)| serde_json::json!({"name": name, "frame": m}))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "samples": samples }))
            .expect("encoding the samples")
    );
}
