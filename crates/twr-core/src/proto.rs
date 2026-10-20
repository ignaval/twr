//! Hub socket protocol: newline-delimited JSON over a Unix socket.
//!
//! - request: `{"id":1,"method":"thread.list","params":{...}}`
//! - response: `{"id":1,"result":...}` or `{"id":1,"error":{"code":"not_found","message":"..."}}`
//! - stream item (subscriptions): `{"stream":"control","gseq":42,"event":{...}}`
//!
//! A subscription request is answered with a normal response, after which the server writes
//! stream items on the same connection until it closes.
//!
//! An error with id 0 is about the connection, which the hub closes next: a line it could not
//! read (`bad_request`), or the hub stopping ([`codes::STOPPING`]). The request a client waits
//! for then was not done ([`crate::client::HubClosed`]).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::event::Event;

/// Largest accepted frame (one JSON line), in bytes.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Largest `blob.get` chunk (`len`), in bytes. Encoded (base64, or JSON-escaped UTF-8) it stays
/// far below [`MAX_FRAME`].
pub const BLOB_CHUNK_MAX: usize = 1024 * 1024;

/// Version of what the hub writes to its clients, sent in `hello` `protocol`
/// ([`crate::client::Client::hello`]). A twr client that sends none, or an older one, cannot
/// decode everything this hub writes, and the hub refuses its `hello` with an update message.
///
/// 1: `turn.started` `from: runtime`. A twr from before it stops reading a stream at the first
/// such turn.
pub const PROTOCOL_VERSION: u32 = 1;

/// Well-known error codes.
pub mod codes {
    /// Malformed request or params.
    pub const BAD_REQUEST: &str = "bad_request";
    /// Unknown method.
    pub const METHOD_NOT_FOUND: &str = "method_not_found";
    /// Unknown thread / attention / object.
    pub const NOT_FOUND: &str = "not_found";
    /// Reference matched several objects.
    pub const AMBIGUOUS: &str = "ambiguous";
    /// The operation is not allowed in the current state.
    pub const CONFLICT: &str = "conflict";
    /// The thread's box has no connected runner.
    pub const BOX_OFFLINE: &str = "box_offline";
    /// The runtime or box cannot do this (e.g. `attach.begin` on a runtime without an
    /// interactive CLI, or a thread with no session yet), or the client is a twr older than
    /// the hub (`hello`, [`super::PROTOCOL_VERSION`]).
    pub const UNSUPPORTED: &str = "unsupported";
    /// The caller's identity is not accepted: a wrong or stale orchestrator token, the
    /// reserved client name `orchestrator` without one, a method the orchestrator may not
    /// call, or a request other than a read from a thread's agent (a process twr started for a
    /// thread, or one of its descendants).
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// The orchestrator tried to answer a permission or question the user did not explicitly
    /// answer in its current turn, or to resolve an item it may only annotate (relay rule).
    pub const RELAY_FORBIDDEN: &str = "relay_forbidden";
    /// The hub is stopping: what it says to a connection it closes as it stops (as the reply
    /// to request 0), so a client that writes to it later knows why it is gone.
    pub const STOPPING: &str = "stopping";
    /// Anything else.
    pub const INTERNAL: &str = "internal";
}

/// A client request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Client-chosen id, echoed in the response.
    pub id: u64,
    /// Method name, e.g. `thread.list`.
    pub method: String,
    /// Parameters (object or null).
    #[serde(default)]
    pub params: Value,
}

/// An error returned by the hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct RpcError {
    /// Machine-readable code (see [`codes`]).
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl RpcError {
    /// Builds an error.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        RpcError {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// A response to a [`Request`].
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// The request id.
    pub id: u64,
    /// `result` or `error`.
    pub result: Result<Value, RpcError>,
}

impl Response {
    /// A success response.
    pub fn ok(id: u64, result: Value) -> Self {
        Response {
            id,
            result: Ok(result),
        }
    }

    /// An error response.
    pub fn err(id: u64, error: RpcError) -> Self {
        Response {
            id,
            result: Err(error),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct WireResponse {
    id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Response", 2)?;
        st.serialize_field("id", &self.id)?;
        match &self.result {
            Ok(v) => st.serialize_field("result", v)?,
            Err(e) => st.serialize_field("error", e)?,
        }
        st.end()
    }
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = WireResponse::deserialize(d)?;
        Ok(Response {
            id: w.id,
            // `"result": null` deserializes as None; treat a missing result as null too.
            result: match w.error {
                Some(e) => Err(e),
                None => Ok(w.result.unwrap_or(Value::Null)),
            },
        })
    }
}

/// Which stream a [`StreamItem`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// Hub control stream (all threads, control kinds only).
    Control,
    /// One thread's full event log.
    Thread,
}

/// One event pushed on a subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamItem {
    /// Stream.
    pub stream: StreamKind,
    /// Global control sequence (control stream only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gseq: Option<u64>,
    /// The event.
    pub event: Event,
}

/// Anything the server writes: a response or a stream item.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// Reply to a request.
    Response(Response),
    /// Subscription push.
    Stream(Box<StreamItem>),
}

impl Serialize for ServerMessage {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ServerMessage::Response(r) => r.serialize(s),
            ServerMessage::Stream(i) => i.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for ServerMessage {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // A message with a `stream` key is a stream item, else a response. Told from its
        // JSON text and parsed from it straight into its type: a subscription's replay is
        // thousands of items, and a `Value` of each costs more than reading it.
        /// Whether the key is there, whatever its value (`null` too).
        fn present<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
            serde::de::IgnoredAny::deserialize(d).map(|_| true)
        }
        #[derive(Deserialize)]
        struct Keys {
            #[serde(default, deserialize_with = "present")]
            stream: bool,
        }
        let raw = Box::<serde_json::value::RawValue>::deserialize(d)?;
        let json = raw.get();
        let direct = serde_json::from_str::<Keys>(json).and_then(|k| {
            if k.stream {
                serde_json::from_str(json).map(|i| ServerMessage::Stream(Box::new(i)))
            } else {
                serde_json::from_str(json).map(ServerMessage::Response)
            }
        });
        if let Ok(msg) = direct {
            return Ok(msg);
        }
        // What does not parse that way (a key given twice, which a `Value` takes the last
        // of) is read as it always was.
        let v: Value = serde_json::from_str(json).map_err(serde::de::Error::custom)?;
        let is_stream = v.get("stream").is_some();
        if is_stream {
            serde_json::from_value(v)
                .map(|i| ServerMessage::Stream(Box::new(i)))
                .map_err(serde::de::Error::custom)
        } else {
            serde_json::from_value(v)
                .map(ServerMessage::Response)
                .map_err(serde::de::Error::custom)
        }
    }
}

/// Serializes `msg` as one NDJSON line (with the trailing newline).
pub fn encode_line<T: Serialize>(msg: &T) -> serde_json::Result<String> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    Ok(s)
}

/// Writes `msg` as one line and flushes.
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize,
{
    let line = encode_line(msg).map_err(std::io::Error::other)?;
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

/// Reads one line and parses it. Blank lines are skipped; `Ok(None)` at EOF. A line whose
/// payload (excluding the trailing newline) is longer than [`MAX_FRAME`] is skipped through its
/// newline and reported as an `InvalidData` error, so the next call starts at the next frame.
/// Malformed JSON is also `InvalidData`.
pub async fn read_frame<R, T>(r: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    read_frame_max(r, MAX_FRAME).await
}

async fn read_frame_max<R, T>(r: &mut R, max: usize) -> std::io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // At most `max` payload bytes plus the newline.
        let n = (&mut *r)
            .take(max as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await?;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        } else if buf.len() > max {
            skip_line(r).await?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame exceeds MAX_FRAME",
            ));
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        return serde_json::from_slice(&buf)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
    }
}

/// Discards input through the next newline (or EOF) without buffering it.
async fn skip_line<R>(r: &mut R) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin + ?Sized,
{
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(());
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => {
                r.consume(i + 1);
                return Ok(());
            }
            None => {
                let n = chunk.len();
                r.consume(n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventBody, SessionPayload, Source};
    use serde_json::json;
    use tokio::io::BufReader;

    #[test]
    fn request_shape() {
        let r = Request {
            id: 1,
            method: "thread.list".into(),
            params: json!({}),
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"id":1,"method":"thread.list","params":{}}"#
        );
        let r: Request = serde_json::from_str(r#"{"id":2,"method":"hub.info"}"#).unwrap();
        assert_eq!(r.params, Value::Null);
    }

    #[test]
    fn response_shapes() {
        let ok = Response::ok(1, json!({"x":1}));
        assert_eq!(
            serde_json::to_string(&ok).unwrap(),
            r#"{"id":1,"result":{"x":1}}"#
        );
        let null = Response::ok(2, Value::Null);
        assert_eq!(
            serde_json::to_string(&null).unwrap(),
            r#"{"id":2,"result":null}"#
        );
        assert_eq!(
            serde_json::from_str::<Response>(r#"{"id":2,"result":null}"#).unwrap(),
            null
        );
        let err = Response::err(3, RpcError::new(codes::NOT_FOUND, "no thread x"));
        let s = serde_json::to_string(&err).unwrap();
        assert_eq!(
            s,
            r#"{"id":3,"error":{"code":"not_found","message":"no thread x"}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), err);
    }

    #[test]
    fn stream_item_shape_and_server_message_dispatch() {
        let ev = crate::event::Event::new(
            "t1",
            1,
            None,
            Source::Hub,
            EventBody::ThreadStopped(SessionPayload::default()),
        );
        let item = StreamItem {
            stream: StreamKind::Control,
            gseq: Some(42),
            event: ev,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["stream"], "control");
        assert_eq!(v["gseq"], 42);
        assert_eq!(v["event"]["kind"], "thread.stopped");
        let m: ServerMessage = serde_json::from_value(v).unwrap();
        assert_eq!(m, ServerMessage::Stream(Box::new(item.clone())));

        let thread_item = StreamItem {
            stream: StreamKind::Thread,
            gseq: None,
            ..item
        };
        assert!(serde_json::to_value(&thread_item)
            .unwrap()
            .get("gseq")
            .is_none());

        let m: ServerMessage = serde_json::from_str(r#"{"id":1,"result":true}"#).unwrap();
        assert_eq!(m, ServerMessage::Response(Response::ok(1, json!(true))));
    }

    /// A message parsed from its text straight into its type is the one a `Value` gives,
    /// and what one way refuses the other refuses: `stream` decides even when `null`, a key
    /// given twice keeps its last value.
    #[test]
    fn server_messages_parse_as_through_a_value() {
        let value_way = |json: &str| -> Result<ServerMessage, String> {
            let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
            if v.get("stream").is_some() {
                serde_json::from_value(v)
                    .map(|i| ServerMessage::Stream(Box::new(i)))
                    .map_err(|e| e.to_string())
            } else {
                serde_json::from_value(v)
                    .map(ServerMessage::Response)
                    .map_err(|e| e.to_string())
            }
        };
        let ev = crate::event::Event::new(
            "t1",
            7,
            Some(2),
            Source::Adapter("codex".into()),
            EventBody::AgentText(crate::event::AgentText {
                text: "hi \u{1F600} \"quoted\"\n".into(),
                ..Default::default()
            }),
        );
        let item = serde_json::to_string(&StreamItem {
            stream: StreamKind::Control,
            gseq: Some(9),
            event: ev,
        })
        .unwrap();
        let cases = [
            item.clone(),
            item.replacen("{", r#"{"gseq":1,"#, 1),
            item.replacen(r#""stream":"control""#, r#""stream":null"#, 1),
            r#"{"id":1,"result":true}"#.to_string(),
            r#"{"id":1,"result":{"a":[1,2]},"id":2}"#.to_string(),
            r#"{"id":3,"error":{"code":-32601,"message":"no such method"}}"#.to_string(),
            r#"{"id":1,"result":true,"stream":null}"#.to_string(),
            r#"{"stream":"thread"}"#.to_string(),
            "[1,2]".to_string(),
            "{".to_string(),
        ];
        for json in &cases {
            let direct = serde_json::from_str::<ServerMessage>(json).map_err(|e| e.to_string());
            match (direct, value_way(json)) {
                (Ok(d), Ok(v)) => assert_eq!(d, v, "{json}"),
                (Err(_), Err(_)) => {}
                (d, v) => panic!("{json}: {d:?} but {v:?}"),
            }
        }
    }

    #[tokio::test]
    async fn frames_round_trip_and_skip_blank_lines() {
        let (a, b) = tokio::io::duplex(64);
        let (ar, mut aw) = tokio::io::split(a);
        let (br, _bw) = tokio::io::split(b);
        let mut br = BufReader::new(br);
        let writer = tokio::spawn(async move {
            write_frame(
                &mut aw,
                &Request {
                    id: 1,
                    method: "a".into(),
                    params: Value::Null,
                },
            )
            .await
            .unwrap();
            aw.write_all(b"\n  \n").await.unwrap();
            write_frame(&mut aw, &Response::ok(1, json!("x".repeat(500))))
                .await
                .unwrap();
            aw.write_all(b"{\"id\":9,\"method\":\"no-newline\"}")
                .await
                .unwrap();
        });
        let r: Request = read_frame(&mut br).await.unwrap().unwrap();
        assert_eq!(r.method, "a");
        let resp: Response = read_frame(&mut br).await.unwrap().unwrap();
        assert_eq!(resp.result.unwrap().as_str().unwrap().len(), 500);
        writer.await.unwrap();
        drop(ar);
        // Final line without newline is still parsed, then EOF.
        let last: Request = read_frame(&mut br).await.unwrap().unwrap();
        assert_eq!(last.id, 9);
    }

    #[tokio::test]
    async fn eof_and_bad_json() {
        let mut empty = BufReader::new(&b""[..]);
        assert!(read_frame::<_, Request>(&mut empty)
            .await
            .unwrap()
            .is_none());
        let mut bad = BufReader::new(&b"{not json}\n"[..]);
        let e = read_frame::<_, Request>(&mut bad).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    }

    fn request_line(len: usize) -> Vec<u8> {
        let empty = serde_json::to_vec(&Request {
            id: 1,
            method: String::new(),
            params: Value::Null,
        })
        .unwrap();
        let method = "m".repeat(len - empty.len());
        let line = serde_json::to_vec(&Request {
            id: 1,
            method,
            params: Value::Null,
        })
        .unwrap();
        assert_eq!(line.len(), len);
        line
    }

    #[tokio::test]
    async fn oversized_frame_is_skipped_through_its_newline() {
        let mut input = request_line(101);
        input.push(b'\n');
        input.extend_from_slice(b"{\"id\":2,\"method\":\"next\"}\n");
        let mut r = BufReader::with_capacity(16, &input[..]);
        let e = read_frame_max::<_, Request>(&mut r, 100).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        let next: Request = read_frame_max(&mut r, 100).await.unwrap().unwrap();
        assert_eq!(next.id, 2);
        assert!(read_frame_max::<_, Request>(&mut r, 100)
            .await
            .unwrap()
            .is_none());

        // Oversized final line without a newline: error, then EOF.
        let input = request_line(150);
        let mut r = BufReader::with_capacity(16, &input[..]);
        assert!(read_frame_max::<_, Request>(&mut r, 100).await.is_err());
        assert!(read_frame_max::<_, Request>(&mut r, 100)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn payload_of_exactly_max_is_accepted_with_or_without_newline() {
        let mut input = request_line(100);
        input.push(b'\n');
        input.extend_from_slice(&request_line(100));
        let mut r = BufReader::with_capacity(16, &input[..]);
        for _ in 0..2 {
            let req: Request = read_frame_max(&mut r, 100).await.unwrap().unwrap();
            assert_eq!(serde_json::to_vec(&req).unwrap().len(), 100);
        }
        assert!(read_frame_max::<_, Request>(&mut r, 100)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn real_max_frame_boundary() {
        let mut input = request_line(MAX_FRAME);
        input.push(b'\n');
        input.extend_from_slice(&request_line(MAX_FRAME + 1));
        input.push(b'\n');
        input.extend_from_slice(b"{\"id\":3,\"method\":\"after\"}\n");
        let mut r = BufReader::new(&input[..]);
        let req: Request = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(req.id, 1);
        assert!(read_frame::<_, Request>(&mut r).await.is_err());
        let after: Request = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(after.id, 3);
    }
}
