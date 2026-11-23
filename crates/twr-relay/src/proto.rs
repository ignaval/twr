//! The hub protocol's messages and frames (docs/relay-protocol.md §2, §5).

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// ALPN of hub protocol version 1.
pub const ALPN_V1: &[u8] = b"twr-relay/1";
/// Every hub protocol version this build speaks, newest first.
pub const HUB_ALPNS: &[&[u8]] = &[ALPN_V1];
/// ALPN of an ACME tls-alpn-01 validation (RFC 8737).
pub const ACME_ALPN: &[u8] = b"acme-tls/1";

pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-twr-relay-hub-auth";
pub const HUB_AUTH_PREFIX: &[u8] = b"twr-relay hub-auth v1\0";
pub const MOVED_PREFIX: &[u8] = b"twr-relay moved v1\0";

/// Frame body limits (§2).
pub const MIN_FRAME: usize = 2;
pub const MAX_FRAME: usize = 65536;
/// The largest moved-note body (§5.7).
pub const MAX_MOVED_BODY: usize = 4096;

/// The bytes a hub signs to authenticate (§5.3).
pub fn hub_auth_message(exporter: &[u8; 32]) -> Vec<u8> {
    [HUB_AUTH_PREFIX, exporter.as_slice()].concat()
}

/// The bytes a hub signs for a moved note (§5.7).
pub fn moved_message(body: &[u8]) -> Vec<u8> {
    [MOVED_PREFIX, body].concat()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneCred {
    /// 16 bytes, 32 hex characters.
    pub id: String,
    /// 32 bytes, base64url.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneRoute {
    pub host: String,
    pub port: u16,
    pub suffix: String,
    pub tags: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRoute {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub phones: u32,
    pub phone_connections: u32,
    pub bytes_per_second: u64,
    pub monthly_bytes: u64,
    pub idle_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub hub: String,
    pub session: String,
    pub server: String,
    pub region: String,
    #[serde(default)]
    pub features: Vec<String>,
    pub phone_routes: Vec<PhoneRoute>,
    pub port_route: Option<PortRoute>,
    pub relay_pin: Option<String>,
    pub iroh_url: Option<String>,
    pub records_url: Option<String>,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Msg {
    Hello {
        spki: String,
        sig: String,
        client: String,
        #[serde(default)]
        features: Vec<String>,
    },
    Welcome(Welcome),
    Phones {
        phones: Vec<PhoneCred>,
    },
    PhonesOk {
        count: u32,
    },
    Moved {
        body: String,
        sig: String,
    },
    MovedOk {},
    Ping {
        n: u64,
    },
    Pong {
        n: u64,
    },
    Drain {
        reason: String,
        retry_after_seconds: u64,
    },
    Error {
        code: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        fatal: bool,
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        ref_: Option<String>,
    },
    /// The header of a phone stream (§5.6).
    Phone {
        phone: Option<String>,
        mode: String,
        remote: String,
    },
}

const KNOWN: &[&str] = &[
    "hello", "welcome", "phones", "phones_ok", "moved", "moved_ok", "ping", "pong", "drain",
    "error", "phone",
];

/// A frame as read: a known message, or an object with a type this build doesn't know.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Msg(Msg),
    Unknown(String),
}

impl Msg {
    pub fn error(code: &str, message: &str, fatal: bool) -> Self {
        Msg::Error { code: code.into(), message: message.into(), fatal, ref_: None }
    }
}

/// Decodes one frame body (§2): one JSON object with a string `type`.
pub fn decode(body: &[u8]) -> Result<Incoming> {
    let value: serde_json::Value = serde_json::from_slice(body)?;
    let Some(obj) = value.as_object() else { bail!("a frame must be a JSON object") };
    let Some(kind) = obj.get("type").and_then(|t| t.as_str()) else {
        bail!("a frame needs a string type")
    };
    if !KNOWN.contains(&kind) {
        return Ok(Incoming::Unknown(kind.chars().take(64).collect()));
    }
    Ok(Incoming::Msg(serde_json::from_value(value)?))
}

pub fn encode(msg: &Msg) -> Vec<u8> {
    serde_json::to_vec(msg).expect("messages serialize")
}

/// Reads one frame. `Ok(None)` at a clean end of stream before a frame starts.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Incoming>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if !(MIN_FRAME..=MAX_FRAME).contains(&len) {
        bail!("frame length {len} out of range");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    decode(&body).map(Some)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg: &Msg) -> Result<()> {
    let body = encode(msg);
    if body.len() > MAX_FRAME {
        bail!("frame too large");
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    w.write_all(&out).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_unknown() {
        let m = Msg::Ping { n: 7 };
        assert_eq!(decode(&encode(&m)).unwrap(), Incoming::Msg(m));
        assert_eq!(
            decode(br#"{"type":"future_thing","x":1}"#).unwrap(),
            Incoming::Unknown("future_thing".into())
        );
        // Unknown members are ignored.
        assert_eq!(
            decode(br#"{"type":"pong","n":3,"later":true}"#).unwrap(),
            Incoming::Msg(Msg::Pong { n: 3 })
        );
        assert!(decode(b"[]").is_err());
        assert!(decode(br#"{"type":3}"#).is_err());
        assert!(decode(br#"{"type":"ping"}"#).is_err());
        let e = Msg::Error { code: "x".into(), message: "m".into(), fatal: true, ref_: Some("t".into()) };
        let text = String::from_utf8(encode(&e)).unwrap();
        assert!(text.contains(r#""ref":"t""#), "{text}");
    }

    #[tokio::test]
    async fn frames_enforce_length() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Msg::Ping { n: 1 }).await.unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r).await.unwrap(), Some(Incoming::Msg(Msg::Ping { n: 1 })));
        assert_eq!(read_frame(&mut r).await.unwrap(), None);
        let mut big = (MAX_FRAME as u32 + 1).to_be_bytes().to_vec();
        big.extend_from_slice(b"{}");
        assert!(read_frame(&mut big.as_slice()).await.is_err());
        let mut tiny = 1u32.to_be_bytes().to_vec();
        tiny.push(b'1');
        assert!(read_frame(&mut tiny.as_slice()).await.is_err());
    }
}
