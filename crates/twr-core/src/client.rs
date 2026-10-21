//! Async client for the hub socket, shared by the CLI, TUI and MCP server.
//!
//! One [`Client`] carries sequential request/response calls. Subscriptions use a dedicated
//! connection: [`Client::subscribe`] consumes the client, waits for the acknowledgement and
//! then yields [`StreamItem`]s.

use std::path::Path;

use anyhow::{anyhow, Context};
use futures::Stream;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::blob::BlobFetch;
use crate::proto::{
    codes, encode_line, read_message, Request, RpcError, ServerMessage, StreamItem, BLOB_CHUNK_MAX,
    PROTOCOL_VERSION,
};

type Reader = BufReader<Box<dyn AsyncRead + Send + Unpin>>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// A connection to the hub.
pub struct Client {
    reader: Reader,
    writer: Writer,
    next_id: u64,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

/// Names a file holding a token that makes a connection the user's whatever its process
/// ancestry, for a hub started with the same variable under `TWR_HOME` (a test's throwaway
/// hub, run from inside a thread whose processes would otherwise count as its agent). A hub
/// without it, or without `TWR_HOME`, ignores the token. Never for real use.
pub const TEST_USER_TOKEN_ENV: &str = "TWR_TEST_USER_TOKEN_FILE";

/// The token [`TEST_USER_TOKEN_ENV`] names, when it names a readable file.
pub fn test_user_token() -> Option<String> {
    let path = std::env::var_os(TEST_USER_TOKEN_ENV)?;
    user_token_of(&std::fs::read_to_string(path).ok()?)
}

/// The test user token a token file's `text` holds: trimmed, 1 to 256 bytes.
pub fn user_token_of(text: &str) -> Option<String> {
    let token = text.trim();
    (!token.is_empty() && token.len() <= 256).then(|| token.to_string())
}

/// The test user token a client's `hello` sends: `token`'s, read only by a build with debug
/// assertions (`debug_build`, the tests' own). A release client never reads the file or sends
/// it: a release hub ignores it anyway, and an agent that names a file of its own gains
/// nothing from a client of the user's.
fn hello_user_token(debug_build: bool, token: impl FnOnce() -> Option<String>) -> Option<String> {
    if debug_build {
        token()
    } else {
        None
    }
}

/// Refuses the hub socket at `path` unless the process listening on it (`peer`: its user id
/// from the connection's credentials, `SO_PEERCRED` / `getpeereid`) runs as this process's
/// effective user `me`. Prompts, answers and the user's identity go to whoever listens there,
/// so another user's socket (a path in a shared directory, `--socket`, `TWR_HOME`) is never
/// used, and neither is one whose peer cannot be told.
fn check_socket_owner(path: &Path, peer: std::io::Result<u32>, me: u32) -> anyhow::Result<()> {
    match peer {
        Ok(uid) if uid == me => Ok(()),
        Ok(uid) => Err(anyhow!(
            "the hub socket {} belongs to uid {uid}, not to you (uid {me}): refusing to use it",
            path.display()
        )),
        Err(e) => Err(anyhow!(
            "cannot tell who listens on the hub socket {}: {e}; refusing to use it",
            path.display()
        )),
    }
}

/// How long a call whose request could not be written waits for what the hub said as it
/// closed the connection ([`parting_words`]): it is already there, or never comes.
const PARTING_READ: std::time::Duration = std::time::Duration::from_secs(1);

/// The hub closed a call's connection before answering it: it stopped or restarted (or
/// crashed) since the connection was opened, or could not read the request. [`Client::call`]
/// returns it (`err.downcast_ref::<HubClosed>()`).
#[derive(Debug)]
pub struct HubClosed {
    /// The method called.
    pub method: String,
    /// Whether the hub may have done it: it got some or all of the request and closed before
    /// it answered. When not, nothing was done ([`HubClosed::resendable`]).
    pub maybe_done: bool,
    /// What the hub said as it closed: its error for the connection (request 0), such as
    /// [`codes::STOPPING`] or a request it could not read ([`codes::BAD_REQUEST`]).
    pub said: Option<RpcError>,
    /// The I/O error that showed the connection was closed, if one did.
    io: Option<std::io::Error>,
}

impl HubClosed {
    /// The error for `method` ([`HubClosed::maybe_done`], [`HubClosed::said`]), with the I/O
    /// error that showed it (`io`).
    pub fn new(
        method: &str,
        maybe_done: bool,
        said: Option<RpcError>,
        io: Option<std::io::Error>,
    ) -> HubClosed {
        HubClosed {
            method: method.to_string(),
            maybe_done,
            said,
            io,
        }
    }

    /// Whether the request can be sent again on a new connection: the hub did nothing with it
    /// and did not refuse it (it closed this connection, it stopped). Even a `thread.send` then
    /// goes once.
    pub fn resendable(&self) -> bool {
        !self.maybe_done && self.said.as_ref().is_none_or(|e| e.code == codes::STOPPING)
    }
}

impl std::fmt::Display for HubClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let method = &self.method;
        match &self.said {
            Some(e) if e.code != codes::STOPPING => {
                return write!(
                    f,
                    "the hub refused the request for {method} and closed the connection: {}",
                    e.message
                );
            }
            _ => {}
        }
        if self.maybe_done {
            write!(
                f,
                "the hub closed the connection before answering {method}; it may have been \
                 done, so check before trying again"
            )?;
        } else {
            write!(
                f,
                "the hub closed the connection before {method} reached it (nothing was done)"
            )?;
        }
        if let Some(e) = &self.said {
            write!(f, ": {}", e.message)?;
        } else if let Some(e) = &self.io {
            write!(f, " ({e})")?;
        }
        write!(f, "; is it running? `twr info`")
    }
}

// Without a source: the I/O error is part of the message, before what to do.
impl std::error::Error for HubClosed {}

/// Whether I/O error `e` on a hub connection means the hub closed it.
fn connection_closed(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        BrokenPipe | ConnectionReset | ConnectionAborted | NotConnected | UnexpectedEof
    )
}

/// The hub's error for a connection it closed (a response to request 0), read from what is
/// left of it.
async fn parting_words(reader: &mut Reader) -> Option<RpcError> {
    loop {
        match read_message(reader).await {
            Ok(Some(ServerMessage::Response(r))) if r.id == 0 => {
                if let Err(e) = r.result {
                    return Some(e);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return None,
        }
    }
}

impl Client {
    /// Connects to the hub socket at `path`. A path too long for a Unix socket fails with a
    /// clear message ([`crate::paths::check_socket_path`]), and so does a socket whose listener
    /// runs as another user ([`check_socket_owner`]): nothing is sent to it.
    pub async fn connect(path: impl AsRef<Path>) -> anyhow::Result<Client> {
        let path = path.as_ref();
        crate::paths::check_socket_path(path)?;
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connecting to hub at {}", path.display()))?;
        check_socket_owner(
            path,
            stream.peer_cred().map(|c| c.uid()),
            crate::paths::effective_uid(),
        )?;
        Ok(Self::from_io(stream))
    }

    /// Wraps any bidirectional byte stream (tests use `tokio::io::duplex`).
    pub fn from_io<S>(io: S) -> Client
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (r, w) = tokio::io::split(io);
        Client {
            reader: BufReader::new(Box::new(r)),
            writer: Box::new(w),
            next_id: 1,
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            id,
            method: method.to_string(),
            params,
        };
        let line = encode_line(&req).context("encoding request")?;
        let mut sent = 0;
        while sent < line.len() {
            let e = match self.writer.write(&line.as_bytes()[sent..]).await {
                Ok(0) => std::io::Error::from(std::io::ErrorKind::WriteZero),
                Ok(n) => {
                    sent += n;
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => e,
            };
            if !connection_closed(&e) && e.kind() != std::io::ErrorKind::WriteZero {
                return Err(anyhow::Error::new(e).context("writing request to hub"));
            }
            // None of it was taken: the hub had closed the connection, and did nothing. (Some
            // of it may be a request the hub reads to its end: then it may have done it.) What
            // the hub said as it hung up may be waiting to be read.
            let said = tokio::time::timeout(PARTING_READ, parting_words(&mut self.reader))
                .await
                .unwrap_or(None);
            return Err(HubClosed::new(method, sent > 0, said, Some(e)).into());
        }
        // Written whole (a socket's flush does nothing, another stream's may), the hub may have
        // it: a failure from here on may come after it was done.
        if let Err(e) = self.writer.flush().await {
            if !connection_closed(&e) {
                return Err(anyhow::Error::new(e).context("writing request to hub"));
            }
            return Err(HubClosed::new(method, true, None, Some(e)).into());
        }
        loop {
            let msg = match read_message(&mut self.reader).await {
                Ok(msg) => msg,
                Err(e) if connection_closed(&e) => {
                    return Err(HubClosed::new(method, true, None, Some(e)).into());
                }
                Err(e) => return Err(anyhow::Error::new(e).context("reading from hub")),
            };
            match msg {
                None => return Err(HubClosed::new(method, true, None, None).into()),
                Some(ServerMessage::Response(r)) if r.id == id => {
                    return r.result.map_err(anyhow::Error::new);
                }
                // The hub's error for the connection (request 0), which it closes next: it is
                // stopping (it read nothing more, this request included), or it could not read
                // the request's line (malformed, or longer than `MAX_FRAME`).
                Some(ServerMessage::Response(r)) if r.id == 0 && r.result.is_err() => {
                    let said = r.result.err();
                    return Err(HubClosed::new(method, false, said, None).into());
                }
                Some(ServerMessage::Response(r)) => {
                    tracing::debug!(id = r.id, "ignoring response to another request");
                }
                Some(ServerMessage::Stream(_)) => {
                    tracing::debug!("ignoring stream item on a call connection");
                }
            }
        }
    }

    /// Calls `method` and returns its result. Hub errors are returned as
    /// [`crate::proto::RpcError`] (use `err.downcast_ref::<RpcError>()` for the code).
    pub async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.request(method, params).await
    }

    /// `hello` with `params` (`client`, `token`) and this build's [`PROTOCOL_VERSION`]. A hub
    /// from before `protocol` refuses the key as unknown: the greeting is made again without
    /// it (such a hub writes nothing this build cannot read).
    pub async fn hello(&mut self, mut params: Value) -> anyhow::Result<Value> {
        params["protocol"] = PROTOCOL_VERSION.into();
        // A test's throwaway hub trusts the test's own clients ([`TEST_USER_TOKEN_ENV`]).
        if let Some(token) = hello_user_token(cfg!(debug_assertions), test_user_token) {
            params["user_token"] = token.into();
        }
        match self.request("hello", params.clone()).await {
            Err(e)
                if e.downcast_ref::<RpcError>().is_some_and(|r| {
                    r.code == codes::BAD_REQUEST && r.message.contains("`protocol`")
                }) =>
            {
                if let Some(p) = params.as_object_mut() {
                    p.remove("protocol");
                }
                self.request("hello", params).await
            }
            r => r,
        }
    }

    /// Like [`Client::call`], deserializing the result into `T`.
    pub async fn call_as<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> anyhow::Result<T> {
        let v = self.request(method, params).await?;
        serde_json::from_value(v).with_context(|| format!("decoding {method} result"))
    }

    /// `summary.thread`: a one-line summary of where a worker thread stands (users only).
    pub async fn summary_thread(
        &mut self,
        params: &crate::proto::SummaryThreadParams,
    ) -> anyhow::Result<crate::proto::SummaryThreadResult> {
        self.call_as("summary.thread", serde_json::to_value(params)?)
            .await
    }

    /// Fetches a whole blob with chunked `blob.get` calls (each at most [`BLOB_CHUNK_MAX`]
    /// bytes). `Ok(None)` when the hub has no such blob.
    pub async fn blob_get(&mut self, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let params = serde_json::json!({
                "ref": blob_ref,
                "offset": out.len(),
                "len": BLOB_CHUNK_MAX,
            });
            let chunk = match self.request("blob.get", params).await {
                Ok(v) => v,
                Err(e)
                    if out.is_empty()
                        && e.downcast_ref::<RpcError>()
                            .is_some_and(|r| r.code == codes::NOT_FOUND) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            let data = chunk["data"].as_str().unwrap_or_default();
            let bytes = match chunk["encoding"].as_str() {
                Some("utf8") => data.as_bytes().to_vec(),
                Some("base64") => crate::b64::decode(data).context("decoding blob chunk")?,
                other => return Err(anyhow!("unknown blob encoding {other:?}")),
            };
            let size = chunk["size"]
                .as_u64()
                .ok_or_else(|| anyhow!("blob.get result without size"))?;
            let eof = chunk["eof"].as_bool().unwrap_or(true);
            if bytes.is_empty() && !eof {
                return Err(anyhow!("blob.get returned an empty chunk before the end"));
            }
            out.extend_from_slice(&bytes);
            if eof {
                if out.len() as u64 != size {
                    return Err(anyhow!(
                        "blob {blob_ref}: got {} bytes, expected {size}",
                        out.len()
                    ));
                }
                return Ok(Some(out));
            }
        }
    }

    /// Turns this connection into a subscription: sends `method` (e.g. `control.subscribe`),
    /// waits for its acknowledgement and returns the stream of pushed items.
    pub async fn subscribe(mut self, method: &str, params: Value) -> anyhow::Result<Subscription> {
        let ack = self.request(method, params).await?;
        Ok(Subscription {
            reader: self.reader,
            _writer: self.writer,
            ack,
        })
    }

    /// Opens a dedicated connection to `path` and subscribes on it.
    pub async fn subscribe_at(
        path: impl AsRef<Path>,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Subscription> {
        Self::connect(path).await?.subscribe(method, params).await
    }
}

impl BlobFetch for Client {
    fn fetch(
        &mut self,
        blob_ref: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<Vec<u8>>>> + Send {
        self.blob_get(blob_ref)
    }
}

/// A live subscription on its own connection. Dropping it closes the connection.
pub struct Subscription {
    reader: Reader,
    // Kept so the server does not see a half-closed connection.
    _writer: Writer,
    ack: Value,
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("ack", &self.ack)
            .finish_non_exhaustive()
    }
}

impl Subscription {
    /// The subscribe call's result.
    pub fn ack(&self) -> &Value {
        &self.ack
    }

    /// Next pushed item; `Ok(None)` when the hub closes the stream.
    pub async fn next(&mut self) -> anyhow::Result<Option<StreamItem>> {
        loop {
            let msg: Option<ServerMessage> = read_message(&mut self.reader)
                .await
                .context("reading subscription")?;
            match msg {
                None => return Ok(None),
                Some(ServerMessage::Stream(item)) => return Ok(Some(*item)),
                Some(ServerMessage::Response(r)) => match r.result {
                    // A late error ends the subscription (e.g. the hub gave up on a lagging client).
                    Err(e) => return Err(anyhow::Error::new(e)),
                    Ok(_) => tracing::debug!(id = r.id, "ignoring response on subscription"),
                },
            }
        }
    }

    /// Converts into a `Stream`, ending after the connection closes or on the first error.
    pub fn into_stream(self) -> impl Stream<Item = anyhow::Result<StreamItem>> + Send {
        futures::stream::unfold(Some(self), |state| async move {
            let mut sub = state?;
            match sub.next().await {
                Ok(Some(item)) => Some((Ok(item), Some(sub))),
                Ok(None) => None,
                Err(e) => Some((Err(e), None)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventBody, SessionPayload, Source};
    use crate::proto::{codes, read_frame, write_frame, Response, RpcError, StreamKind};
    use futures::StreamExt;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A release client never reads the test token file, let alone sends it; a debug one
    /// sends what the file holds, trimmed.
    #[test]
    fn only_debug_clients_send_the_test_token() {
        let never = || -> Option<String> { panic!("a release client read the token") };
        assert_eq!(hello_user_token(false, never), None);
        assert_eq!(
            hello_user_token(true, || user_token_of(" t0k\n")),
            Some("t0k".to_string())
        );
        assert_eq!(user_token_of(" \n"), None);
        assert_eq!(user_token_of(&"x".repeat(257)), None);
    }

    /// A client and the hub's end of its connection.
    fn pair() -> (Client, tokio::net::UnixStream) {
        let (c, h) = tokio::net::UnixStream::pair().unwrap();
        (Client::from_io(c), h)
    }

    fn line(r: &Response) -> Vec<u8> {
        let mut l = serde_json::to_vec(r).unwrap();
        l.push(b'\n');
        l
    }

    fn stopping() -> Response {
        Response::err(0, RpcError::new(codes::STOPPING, "the hub is stopping"))
    }

    async fn closed_on(c: &mut Client) -> (HubClosed, String) {
        let e = c
            .call("thread.send", json!({"thread": "w1", "prompt": "hi"}))
            .await
            .unwrap_err();
        let shown = format!("{e:#}");
        let closed = e.downcast::<HubClosed>().expect("a HubClosed");
        (closed, shown)
    }

    /// A connection the hub closed as it stopped (the TUI's, the MCP server's, kept open):
    /// the next call cannot be written, so nothing was done, and the error says why the hub
    /// went (what it said as it closed) and what to check, not "Broken pipe".
    #[tokio::test]
    async fn a_call_on_a_connection_the_hub_closed_says_so() {
        let (mut c, mut hub) = pair();
        hub.write_all(&line(&stopping())).await.unwrap();
        drop(hub);
        let (closed, shown) = closed_on(&mut c).await;
        assert!(!closed.maybe_done && closed.resendable(), "{closed:?}");
        assert_eq!(closed.said.as_ref().unwrap().code, codes::STOPPING);
        assert_eq!(
            shown,
            "the hub closed the connection before thread.send reached it (nothing was done): \
             the hub is stopping; is it running? `twr info`"
        );

        // A hub that said nothing (one that crashed, or older than the notice).
        let (mut c, hub) = pair();
        drop(hub);
        let (closed, shown) = closed_on(&mut c).await;
        assert!(closed.resendable(), "{closed:?}");
        assert!(
            shown.starts_with(
                "the hub closed the connection before thread.send reached it (nothing was \
                 done) (Broken pipe"
            ) && shown.ends_with("; is it running? `twr info`"),
            "{shown}"
        );
    }

    /// The hub read the request and closed before it answered: it may have been done, so it
    /// is not sent again, and the error says to check.
    #[tokio::test]
    async fn a_call_the_hub_had_is_not_sent_again() {
        let (mut c, hub) = pair();
        let reader = tokio::spawn(async move {
            let mut r = BufReader::new(hub);
            let mut got = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut r, &mut got)
                .await
                .unwrap();
            got
        });
        let (closed, shown) = closed_on(&mut c).await;
        assert!(reader.await.unwrap().contains("thread.send"));
        assert!(closed.maybe_done && !closed.resendable(), "{closed:?}");
        assert_eq!(
            shown,
            "the hub closed the connection before answering thread.send; it may have been \
             done, so check before trying again; is it running? `twr info`"
        );
    }

    /// A stream that takes every byte, then fails to flush: the hub may have the request.
    struct FlushFails(tokio::io::DuplexStream);

    impl AsyncRead for FlushFails {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for FlushFails {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }
        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    /// A stream that takes as many bytes as it holds, then fails.
    struct TakesSome(usize);

    impl AsyncRead for TakesSome {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for TakesSome {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let n = self.0.min(buf.len());
            self.0 -= n;
            std::task::Poll::Ready(if n == 0 {
                Err(std::io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(n)
            })
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A request the stream took part of (all but its newline, say: a hub reads a last line
    /// without one) may have been done; one it took none of was not.
    #[tokio::test]
    async fn only_a_request_none_of_which_was_taken_is_resendable() {
        let (closed, _) = closed_on(&mut Client::from_io(TakesSome(0))).await;
        assert!(closed.resendable(), "{closed:?}");
        let line = encode_line(&Request {
            id: 1,
            method: "thread.send".into(),
            params: json!({"thread": "w1", "prompt": "hi"}),
        })
        .unwrap();
        for taken in [1, line.len() - 1] {
            let (closed, _) = closed_on(&mut Client::from_io(TakesSome(taken))).await;
            assert!(
                closed.maybe_done && !closed.resendable(),
                "{taken}: {closed:?}"
            );
        }
    }

    /// A request written whole whose flush fails may have been done: never resent.
    #[tokio::test]
    async fn a_failed_flush_after_a_whole_request_is_not_resendable() {
        let (c, _hub) = tokio::io::duplex(1 << 16);
        let mut c = Client::from_io(FlushFails(c));
        let (closed, _) = closed_on(&mut c).await;
        assert!(closed.maybe_done && !closed.resendable(), "{closed:?}");
    }

    /// A request the hub could not read (an oversized line, malformed JSON) is refused with
    /// its error for the connection (request 0), which the client used to skip.
    #[tokio::test]
    async fn a_request_the_hub_cannot_read_is_refused_with_why() {
        let (mut c, hub) = pair();
        tokio::spawn(async move {
            let mut r = BufReader::new(hub);
            let mut got = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut r, &mut got)
                .await
                .unwrap();
            let e = RpcError::new(codes::BAD_REQUEST, "frame exceeds MAX_FRAME");
            r.get_mut()
                .write_all(&line(&Response::err(0, e)))
                .await
                .unwrap();
        });
        let (closed, shown) = closed_on(&mut c).await;
        assert!(!closed.maybe_done && !closed.resendable(), "{closed:?}");
        assert_eq!(
            shown,
            "the hub refused the request for thread.send and closed the connection: frame \
             exceeds MAX_FRAME"
        );
    }

    fn item(seq: u64) -> StreamItem {
        StreamItem {
            stream: StreamKind::Control,
            gseq: Some(seq),
            event: Event::new(
                "t1",
                seq,
                None,
                Source::Hub,
                EventBody::ThreadStarted(SessionPayload::default()),
            ),
        }
    }

    /// A tiny scripted server keyed on method name.
    async fn serve<S>(io: S, subscribe_items: Vec<StreamItem>)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (r, mut w) = tokio::io::split(io);
        let mut r = BufReader::new(r);
        while let Some(req) = read_frame::<_, Request>(&mut r).await.unwrap() {
            match req.method.as_str() {
                "echo" => {
                    // Noise first: a stray stream item and a response to another id.
                    write_frame(&mut w, &item(0)).await.unwrap();
                    write_frame(&mut w, &Response::ok(999, json!("wrong")))
                        .await
                        .unwrap();
                    write_frame(&mut w, &Response::ok(req.id, req.params))
                        .await
                        .unwrap();
                }
                "fail" => {
                    write_frame(
                        &mut w,
                        &Response::err(req.id, RpcError::new(codes::NOT_FOUND, "nope")),
                    )
                    .await
                    .unwrap();
                }
                "control.subscribe" => {
                    write_frame(&mut w, &Response::ok(req.id, json!({"since": 0})))
                        .await
                        .unwrap();
                    for it in &subscribe_items {
                        write_frame(&mut w, it).await.unwrap();
                    }
                    w.shutdown().await.unwrap();
                    return;
                }
                "blob.get" => {
                    // A 2.5-chunk blob of binary bytes served in base64 and UTF-8 chunks.
                    let blob = test_blob();
                    let off = req.params["offset"].as_u64().unwrap() as usize;
                    let len = req.params["len"].as_u64().unwrap() as usize;
                    let resp = if req.params["ref"] == "sha256:none" {
                        Response::err(req.id, RpcError::new(codes::NOT_FOUND, "no blob"))
                    } else {
                        let end = (off + len).min(blob.len());
                        let part = &blob[off..end];
                        let (enc, data) = match std::str::from_utf8(part) {
                            Ok(s) => ("utf8", s.to_string()),
                            Err(_) => ("base64", crate::b64::encode(part)),
                        };
                        Response::ok(
                            req.id,
                            json!({"ref": req.params["ref"], "size": blob.len(), "offset": off,
                                "encoding": enc, "data": data, "eof": end == blob.len()}),
                        )
                    };
                    write_frame(&mut w, &resp).await.unwrap();
                }
                "hangup" => return,
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn call_skips_noise_and_matches_ids() {
        let (a, b) = tokio::io::duplex(1024);
        tokio::spawn(serve(b, vec![]));
        let mut c = Client::from_io(a);
        assert_eq!(
            c.call("echo", json!({"a": 1})).await.unwrap(),
            json!({"a": 1})
        );
        let n: u32 = c.call_as("echo", json!(7)).await.unwrap();
        assert_eq!(n, 7);
        let err = c.call("fail", Value::Null).await.unwrap_err();
        let rpc = err.downcast_ref::<RpcError>().unwrap();
        assert_eq!(rpc.code, codes::NOT_FOUND);
        assert!(c.call("hangup", Value::Null).await.is_err());
    }

    fn test_blob() -> Vec<u8> {
        let mut b = vec![b'a'; BLOB_CHUNK_MAX];
        b.extend((0..=255u8).cycle().take(BLOB_CHUNK_MAX));
        b.extend(vec![b'z'; BLOB_CHUNK_MAX / 2]);
        b
    }

    #[tokio::test]
    async fn blob_get_assembles_chunks() {
        let (a, b) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve(b, vec![]));
        let mut c = Client::from_io(a);
        let got = c.blob_get("sha256:x").await.unwrap().unwrap();
        assert_eq!(got, test_blob());
        assert!(c.blob_get("sha256:none").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn subscription_yields_items_then_ends() {
        let (a, b) = tokio::io::duplex(1024);
        tokio::spawn(serve(b, vec![item(1), item(2), item(3)]));
        let mut sub = Client::from_io(a)
            .subscribe("control.subscribe", json!({"since": 0}))
            .await
            .unwrap();
        assert_eq!(sub.ack(), &json!({"since": 0}));
        assert_eq!(sub.next().await.unwrap().unwrap().gseq, Some(1));
        let rest: Vec<_> = sub.into_stream().collect().await;
        let gseqs: Vec<_> = rest.into_iter().map(|r| r.unwrap().gseq).collect();
        assert_eq!(gseqs, vec![Some(2), Some(3)]);
    }

    #[tokio::test]
    async fn subscription_error_after_ack_ends_stream() {
        let (a, b) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(b);
            let mut r = BufReader::new(r);
            let req: Request = read_frame(&mut r).await.unwrap().unwrap();
            write_frame(&mut w, &Response::ok(req.id, Value::Null))
                .await
                .unwrap();
            write_frame(&mut w, &item(1)).await.unwrap();
            write_frame(
                &mut w,
                &Response::err(req.id, RpcError::new("lagged", "too slow")),
            )
            .await
            .unwrap();
            // Keep the connection open; the client must stop on the error alone.
            let mut sink = Vec::new();
            let _ = r.read_to_end(&mut sink).await;
        });
        let sub = Client::from_io(a)
            .subscribe("control.subscribe", Value::Null)
            .await
            .unwrap();
        let items: Vec<_> = sub.into_stream().collect().await;
        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok());
        assert!(items[1].is_err());
    }

    #[tokio::test]
    async fn over_a_real_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            loop {
                let (conn, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(conn, vec![item(5)]));
            }
        });
        let mut c = Client::connect(&path).await.unwrap();
        assert_eq!(c.call("echo", json!("hi")).await.unwrap(), json!("hi"));
        let mut sub = Client::subscribe_at(&path, "control.subscribe", Value::Null)
            .await
            .unwrap();
        assert_eq!(sub.next().await.unwrap().unwrap().gseq, Some(5));
        assert!(sub.next().await.unwrap().is_none());
        // The call connection is independent of the subscription.
        assert_eq!(c.call("echo", json!(1)).await.unwrap(), json!(1));
    }

    /// A hub from before `protocol` decodes `hello` params as below and answers a decoding
    /// error as `bad_request` with serde's message: the greeting is made again without it.
    #[tokio::test]
    async fn hello_greets_a_hub_from_before_protocol_without_it() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct HelloParams {
            client: String,
            token: Option<String>,
        }
        let (a, b) = tokio::io::duplex(1024);
        let server = tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(b);
            let mut r = BufReader::new(r);
            let mut seen = Vec::new();
            while let Some(req) = read_frame::<_, Request>(&mut r).await.unwrap() {
                seen.push(req.params.clone());
                let resp = match serde_json::from_value::<HelloParams>(req.params) {
                    Ok(p) => Response::ok(req.id, json!({"client": p.client})),
                    Err(e) => {
                        Response::err(req.id, RpcError::new(codes::BAD_REQUEST, e.to_string()))
                    }
                };
                write_frame(&mut w, &resp).await.unwrap();
            }
            seen
        });
        let mut c = Client::from_io(a);
        let hello = c.hello(json!({"client": "tui"})).await.unwrap();
        assert_eq!(hello["client"], "tui");
        drop(c);
        assert_eq!(
            server.await.unwrap(),
            vec![
                json!({"client": "tui", "protocol": PROTOCOL_VERSION}),
                json!({"client": "tui"}),
            ]
        );
    }

    #[tokio::test]
    async fn connect_to_missing_socket_fails() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Client::connect(dir.path().join("nope.sock")).await.is_err());
    }

    #[test]
    fn a_socket_of_another_user_is_refused() {
        let path = Path::new("/run/user/1000/twr/hub.sock");
        assert!(check_socket_owner(path, Ok(1000), 1000).is_ok());
        let e = check_socket_owner(path, Ok(0), 1000)
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "the hub socket /run/user/1000/twr/hub.sock belongs to uid 0, not to you (uid 1000): \
             refusing to use it"
        );
        assert!(check_socket_owner(path, Ok(1001), 1000).is_err());
        // Credentials that cannot be read: refused too.
        let e = check_socket_owner(path, Err(std::io::Error::other("no credentials")), 1000)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no credentials") && e.contains("refusing"),
            "{e}"
        );
    }
}
