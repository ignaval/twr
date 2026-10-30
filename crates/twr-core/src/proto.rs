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
        let v = Value::deserialize(d)?;
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

impl ServerMessage {
    /// One line the hub wrote (without its newline), read as its `Deserialize` reads it, but
    /// from the text straight into its types, a stream item's payload too: a subscription's
    /// replay is thousands of items, and a `Value` of each costs more than reading it. What
    /// does not parse that way is read the `Value` way, so what that accepts is built the same
    /// ([`crate::event::EventBody::from_kind_json`] says what else it takes).
    pub fn from_line(line: &[u8]) -> serde_json::Result<ServerMessage> {
        /// Whether the key is there, whatever its value (`null` too).
        fn present<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
            serde::de::IgnoredAny::deserialize(d).map(|_| true)
        }
        #[derive(Deserialize)]
        struct Keys {
            #[serde(default, deserialize_with = "present")]
            stream: bool,
        }
        #[derive(Deserialize)]
        struct Item<'a> {
            stream: StreamKind,
            #[serde(default)]
            gseq: Option<u64>,
            #[serde(borrow)]
            event: crate::event::WireEventText<'a>,
        }
        let direct = serde_json::from_slice::<Keys>(line).and_then(|k| {
            if k.stream {
                let i: Item = serde_json::from_slice(line)?;
                Ok(ServerMessage::Stream(Box::new(StreamItem {
                    stream: i.stream,
                    gseq: i.gseq,
                    event: i.event.into_event()?,
                })))
            } else {
                serde_json::from_slice(line).map(ServerMessage::Response)
            }
        });
        direct.or_else(|_| serde_json::from_slice(line))
    }
}

/// [`read_frame`] of what the hub writes, read with [`ServerMessage::from_line`].
pub async fn read_message<R>(r: &mut R) -> std::io::Result<Option<ServerMessage>>
where
    R: AsyncBufRead + Unpin + ?Sized,
{
    read_line_max(r, MAX_FRAME, ServerMessage::from_line).await
}

/// `summary.thread`'s parameters (users only; `twr-hub` `hub::summary`): a one-line summary of
/// where worker `thread` stands, from the small model `models` names for the runtime that
/// answers (each runtime's default when left out), at most one per `every_secs` (60 at least,
/// ten years at most: more is `bad_request`); `retry` after the user turned summaries on again.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryThreadParams {
    /// The thread, by id, name or alias.
    pub thread: String,
    /// The model for each runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<crate::config::SuggestModels>,
    /// The shortest time between two summaries of the thread, in seconds (`[tui]
    /// summary_every`).
    pub every_secs: u64,
    /// The user turned summaries on again (`/summaries on`): the hub probes the agents' CLIs
    /// afresh and tries again what calls (of summaries or suggestions) found unable to run,
    /// as `suggest.next`'s `retry` does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retry: bool,
}

/// `summary.thread`'s result. `summary` is the thread's summary (`None`: none, `reason` says
/// why); `basis` names what this ask read of the thread (a hash: the same basis gets the same
/// summary); `covers` is the routine update the summary is of (the newest it read: `seq`
/// `covers` of the orchestrator's thread `covers_in`; an unchanged basis, this ask's newest);
/// `runtime` and `model` answered it; `cached`: no model was asked for this one; `next_after_secs`: ask
/// again no sooner than this.
///
/// `reason` (absent with a fresh or cached summary): `none` (the model had nothing to say),
/// `interval` (the last summary is younger than `every_secs`: `summary` is that one, made
/// from an earlier basis, of the updates up to `covers`; also the answer to an ask that waited
/// on a call made of another basis),
/// `not_worker` (the orchestrator), `limit` (the hub's hourly cap), `backoff` (a call failed a
/// moment ago), `failed: ...`, `unavailable: ...` (no runtime can run it here: no CLI, or none
/// signed in; [`SummaryThreadResult::unavailable`]), `retry: ...` (this runtime can't run, another
/// may for the next ask).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SummaryThreadResult {
    /// The thread's id.
    pub thread: String,
    /// The summary: one plain line.
    pub summary: Option<String>,
    /// What this ask read of the thread, as a hash (with `interval`, the summary was made from
    /// an earlier one).
    pub basis: String,
    /// The `seq` (in the orchestrator's thread, `covers_in`) of the newest routine update the
    /// summary read: the run of updates it is of. `None`: it read none, or there is no summary
    /// (and from a hub before it was given).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers: Option<u64>,
    /// The orchestrator's conversation (its thread's id) the summary read the updates in:
    /// `covers` is a `seq` of that thread's. A reset or a switch of the orchestrator is a new
    /// thread whose seqs start again, so an update is told by both. `None`: there was no
    /// conversation to read, or no summary was made or kept to answer with (the hub held off,
    /// a call failed), and from a hub before it was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers_in: Option<String>,
    /// The runtime that answered (`claude-code`, `codex`).
    pub runtime: Option<String>,
    /// The model that answered.
    pub model: Option<String>,
    /// Kept from before: no model was asked.
    pub cached: bool,
    /// Why there is no fresh summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Asking again sooner gets the same answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_secs: Option<u64>,
    /// The hub's summary calls since it started.
    pub stats: SummaryStats,
}

impl SummaryThreadResult {
    /// Whether no runtime can run summaries here (no CLI, or none signed in): asking again
    /// gets the same.
    pub fn unavailable(&self) -> bool {
        self.reason
            .as_deref()
            .is_some_and(|r| r.starts_with("unavailable: "))
    }
}

/// The hub's summary calls since it started (`summary.thread` `stats`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SummaryStats {
    /// Calls made.
    pub calls: u64,
    /// Calls that failed.
    pub failures: u64,
    /// What they cost at list price, in US dollars.
    pub cost_usd: f64,
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
    read_line_max(r, max, |line| serde_json::from_slice(line)).await
}

/// [`read_frame_max`] with each line parsed by `parse`.
async fn read_line_max<R, T>(
    r: &mut R,
    max: usize,
    parse: impl Fn(&[u8]) -> serde_json::Result<T>,
) -> std::io::Result<Option<T>>
where
    R: AsyncBufRead + Unpin + ?Sized,
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
        return parse(&buf)
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

    /// `covers_in` is new: an older hub's answer (no `covers_in`) reads as `None`, an answer
    /// without it is written without it, and one with it goes both ways.
    #[test]
    fn a_summary_result_carries_the_conversation_its_covers_is_in() {
        let old: SummaryThreadResult =
            serde_json::from_str(r#"{"thread":"api","summary":"ok","basis":"b","covers":7}"#)
                .unwrap();
        assert_eq!((old.covers, old.covers_in.as_deref()), (Some(7), None));
        let v = serde_json::to_value(&old).unwrap();
        assert!(v.get("covers_in").is_none(), "{v}");
        let new = SummaryThreadResult {
            covers_in: Some("o2".into()),
            ..old
        };
        let v = serde_json::to_value(&new).unwrap();
        assert_eq!((&v["covers"], &v["covers_in"]), (&json!(7), &json!("o2")));
        assert_eq!(
            serde_json::from_value::<SummaryThreadResult>(v).unwrap(),
            new
        );
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

    /// A message parsed from its text straight into its type ([`ServerMessage::from_line`])
    /// is the one a `Value` gives, and what one way refuses the other refuses: `stream`
    /// decides even when `null`, a key given twice keeps its last value. The `Deserialize`
    /// impls stay the `Value` way, for any deserializer: inside an untagged enum (which
    /// buffers what it reads), and through `from_value` without touching a number.
    #[test]
    fn server_messages_parse_as_through_a_value() {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Message(ServerMessage),
            #[allow(dead_code)]
            Other(u8),
        }
        let ev = crate::event::Event::new(
            "t1",
            1,
            None,
            Source::Hub,
            EventBody::TurnEnded(crate::event::TurnEnded::default()),
        );
        let line = serde_json::to_string(&StreamItem {
            stream: StreamKind::Thread,
            gseq: None,
            event: ev.clone(),
        })
        .unwrap();
        let Either::Message(ServerMessage::Stream(item)) =
            serde_json::from_str::<Either>(&line).unwrap()
        else {
            panic!("not a stream item");
        };
        assert_eq!(item.event, ev);
        let m: ServerMessage =
            serde_json::from_value(json!({"id": 1, "result": 51.248178375505404})).unwrap();
        assert_eq!(
            m,
            ServerMessage::Response(Response::ok(1, json!(51.248178375505404)))
        );

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
            let direct = ServerMessage::from_line(json.as_bytes()).map_err(|e| e.to_string());
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
