//! Driving a yamux connection from one task, for either side (docs/relay-protocol.md §5.1).

use futures::future::poll_fn;
use std::collections::VecDeque;
use std::task::Poll;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::sync::CancellationToken;

/// A yamux stream as a Tokio byte stream.
pub type MuxStream = Compat<yamux::Stream>;

pub type OpenRequest = oneshot::Sender<Result<MuxStream, yamux::ConnectionError>>;

/// The relay side's window: 32 streams of 256 KiB each at least, 16 MiB per connection, so
/// that many idle hubs cost little memory.
pub fn relay_config() -> yamux::Config {
    let mut c = yamux::Config::default();
    c.set_max_num_streams(32);
    c.set_max_connection_receive_window(Some(16 * 1024 * 1024));
    c
}

pub fn hub_config() -> yamux::Config {
    let mut c = yamux::Config::default();
    c.set_max_num_streams(64);
    c.set_max_connection_receive_window(Some(64 * 1024 * 1024));
    c
}

/// Runs `io` as a yamux connection until it closes or `cancel` fires. Streams the peer opens
/// go to `inbound`; requests on `open` get a new outbound stream each.
pub async fn drive<T>(
    io: T,
    cfg: yamux::Config,
    mode: yamux::Mode,
    mut open: mpsc::Receiver<OpenRequest>,
    inbound: mpsc::Sender<MuxStream>,
    cancel: CancellationToken,
) -> Result<(), yamux::ConnectionError>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut conn = yamux::Connection::new(io.compat(), cfg, mode);
    let mut pending: VecDeque<OpenRequest> = VecDeque::new();
    let mut open_closed = false;
    let cancelled = cancel.cancelled();
    tokio::pin!(cancelled);
    let result = poll_fn(|cx| {
        if cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ok(()));
        }
        while !open_closed {
            match open.poll_recv(cx) {
                Poll::Ready(Some(req)) => pending.push_back(req),
                Poll::Ready(None) => open_closed = true,
                Poll::Pending => break,
            }
        }
        while !pending.is_empty() {
            match conn.poll_new_outbound(cx) {
                Poll::Ready(Ok(s)) => {
                    let req = pending.pop_front().expect("non-empty");
                    let _ = req.send(Ok(s.compat()));
                }
                Poll::Ready(Err(e)) => {
                    for req in pending.drain(..) {
                        let _ = req.send(Err(yamux::ConnectionError::Closed));
                    }
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => break,
            }
        }
        loop {
            match conn.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(s))) => {
                    // A full queue means the reader is gone or swamped: drop (reset) the stream.
                    let _ = inbound.try_send(s.compat());
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    })
    .await;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        poll_fn(|cx| conn.poll_close(cx)),
    )
    .await;
    result
}

use std::future::Future as _;
