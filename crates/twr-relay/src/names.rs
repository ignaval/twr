//! Pins, hub names, phone tags and SNI labels (docs/relay-protocol.md §3).

use crate::b32;
use base64::Engine as _;
use ring::{digest, hmac};

/// Length of a hub name and of a tag, in characters.
pub const NAME_LEN: usize = 26;
/// Length of a tag window, in seconds.
pub const WINDOW_SECS: u64 = 300;
/// The tag versions this build can compute and check.
pub const TAG_VERSIONS: &[u32] = &[1];

const HUB_NAME_LABEL: &[u8] = b"twr-relay hub-name v1\0";
const TAG_LABEL: &[u8] = b"twr-relay tag v1\0";

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text).ok()
}

/// SHA-256 of a DER SubjectPublicKeyInfo (§3.1).
pub fn pin(spki_der: &[u8]) -> [u8; 32] {
    let d = digest::digest(&digest::SHA256, spki_der);
    d.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

/// A pin as base64url, 43 characters.
pub fn pin_text(pin: &[u8; 32]) -> String {
    b64(pin)
}

/// The inverse of [`pin_text`].
pub fn parse_pin(text: &str) -> Option<[u8; 32]> {
    if text.len() != 43 {
        return None;
    }
    unb64(text)?.try_into().ok()
}

/// A hub's relay name, from its pin (§3.2).
pub fn hub_name(pin: &[u8; 32]) -> String {
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(HUB_NAME_LABEL);
    ctx.update(pin);
    b32::encode(&ctx.finish().as_ref()[..16])
}

/// The tag window for a Unix time.
pub fn window(unix_secs: u64) -> u64 {
    unix_secs / WINDOW_SECS
}

/// A phone's tag for one window (§3.3).
pub fn tag(relay_key: &[u8; 32], hub_name: &str, window: u64) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, relay_key);
    let mut ctx = hmac::Context::with_key(&key);
    ctx.update(TAG_LABEL);
    ctx.update(hub_name.as_bytes());
    ctx.update(&window.to_be_bytes());
    b32::encode(&ctx.sign().as_ref()[..16])
}

/// Whether `tag` is valid for `relay_key` in `window` or a neighbour (§3.3), in constant time
/// for each candidate.
pub fn tag_matches(relay_key: &[u8; 32], hub_name: &str, tag_text: &str, window: u64) -> bool {
    let mut ok = false;
    for w in [window.wrapping_sub(1), window, window.wrapping_add(1)] {
        let want = tag(relay_key, hub_name, w);
        ok |= ct_eq(want.as_bytes(), tag_text.as_bytes());
    }
    ok
}

/// Compares two byte strings in time that depends only on their lengths.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// The first SNI label, `<hub_name>-<tag>`.
pub fn label(hub_name: &str, tag: &str) -> String {
    format!("{hub_name}-{tag}")
}

/// A phone's full server name (§3.3).
pub fn phone_sni(hub_name: &str, tag: &str, suffix: &str) -> String {
    format!("{hub_name}-{tag}.{suffix}")
}

/// Splits a `<hub_name>-<tag>` label, checking both halves' form.
pub fn parse_label(label: &str) -> Option<(&str, &str)> {
    if label.len() != NAME_LEN * 2 + 1 || label.as_bytes()[NAME_LEN] != b'-' {
        return None;
    }
    let (hub, tag) = (&label[..NAME_LEN], &label[NAME_LEN + 1..]);
    (b32::is_label(hub, NAME_LEN) && b32::is_label(tag, NAME_LEN)).then_some((hub, tag))
}

/// Whether a hub name has the right form.
pub fn is_hub_name(text: &str) -> bool {
    b32::is_label(text, NAME_LEN)
}

/// Splits a server name into a phone label and a suffix, if its first label is one.
pub fn split_phone_sni(sni: &str) -> Option<(&str, &str, &str)> {
    let (first, rest) = sni.split_once('.')?;
    let (hub, tag) = parse_label(first)?;
    Some((hub, tag, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_tag_shapes() {
        let pin = [1u8; 32];
        let name = hub_name(&pin);
        assert_eq!(name.len(), NAME_LEN);
        assert!(is_hub_name(&name));
        let t = tag(&[2u8; 32], &name, 5_000_000);
        assert_eq!(t.len(), NAME_LEN);
        let l = label(&name, &t);
        assert_eq!(l.len(), 53);
        assert_eq!(parse_label(&l), Some((name.as_str(), t.as_str())));
        let sni = phone_sni(&name, &t, "eu.relay.twr.tools");
        assert_eq!(split_phone_sni(&sni), Some((name.as_str(), t.as_str(), "eu.relay.twr.tools")));
    }

    #[test]
    fn tag_window_tolerance() {
        let key = [9u8; 32];
        let name = hub_name(&[3u8; 32]);
        let w = window(1_800_000_000);
        let t = tag(&key, &name, w);
        assert!(tag_matches(&key, &name, &t, w));
        assert!(tag_matches(&key, &name, &t, w + 1));
        assert!(tag_matches(&key, &name, &t, w - 1));
        assert!(!tag_matches(&key, &name, &t, w + 2));
        assert!(!tag_matches(&key, &name, &t, w - 2));
        assert!(!tag_matches(&[8u8; 32], &name, &t, w));
        assert!(!tag_matches(&key, &hub_name(&[4u8; 32]), &t, w));
    }

    #[test]
    fn label_parsing_refuses_near_misses() {
        let good = format!("{}-{}", "a".repeat(26), "b".repeat(26));
        assert!(parse_label(&good).is_some());
        assert!(parse_label(&good.to_uppercase()).is_none());
        assert!(parse_label(&format!("{}-{}", "a".repeat(25), "b".repeat(27))).is_none());
        assert!(parse_label(&format!("{}_{}", "a".repeat(26), "b".repeat(26))).is_none());
        assert!(parse_label(&format!("{}-{}", "a".repeat(26), "b".repeat(25))).is_none());
        assert!(parse_label(&format!("{}-{}1", "a".repeat(26), "b".repeat(25))).is_none());
        assert!(parse_label("").is_none());
        // A multi-byte character at the split position must not panic.
        let odd = format!("{}é{}", "a".repeat(25), "b".repeat(26));
        assert!(parse_label(&odd).is_none());
    }

    #[test]
    fn pin_text_roundtrip() {
        let p = pin(b"not really an spki");
        let t = pin_text(&p);
        assert_eq!(t.len(), 43);
        assert_eq!(parse_pin(&t), Some(p));
        assert!(parse_pin(&format!("{t}=")).is_none());
        assert!(parse_pin(&t[..42]).is_none());
    }
}
