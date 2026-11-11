//! What a peer that has not authenticated can hold or spend: connections kept open before a
//! paired device's socket upgrade, pairing attempts and refused bearer upgrades. Limits are
//! per peer (an IPv4 address or an IPv6 /64) under a global ceiling, so one source cannot
//! lock everyone else out.
use axum_server::accept::Accept;
use std::{
    collections::HashMap,
    future::Future,
    io,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

/// The peer that limits count against: an IPv4 address (IPv4-mapped IPv6 included) or an
/// IPv6 /64, which one host or network usually holds whole.
pub fn peer_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
        v4 => v4,
    }
}

/// Bounds on connections that have not become an authenticated device socket.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Unauthenticated connections open at once, TLS handshakes included.
    pub total: usize,
    /// The same, from one peer.
    pub per_peer: usize,
    /// A connection that sends no request byte within this is closed (h2 and HTTP version
    /// detection have no header timeout of their own).
    pub first_byte: Duration,
    /// An unauthenticated connection is closed after this, however busy.
    pub lifetime: Duration,
    /// hyper's own bound on reading one HTTP/1 request head, idle keep-alive included.
    pub header_read: Duration,
}
pub const PRE_AUTH: Limits = Limits {
    total: 128,
    per_peer: 16,
    first_byte: Duration::from_secs(10),
    lifetime: Duration::from_secs(120),
    header_read: Duration::from_secs(10),
};

#[derive(Default)]
struct Open {
    per_peer: HashMap<IpAddr, usize>,
    /// Each unauthenticated connection by its peer address, with its admission number.
    pending: HashMap<SocketAddr, u64>,
}
pub struct Connections {
    limits: Limits,
    open: Mutex<Open>,
    next: AtomicU64,
}
impl Connections {
    pub fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            open: Mutex::new(Open::default()),
            next: AtomicU64::new(0),
        })
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }
    /// Count a new connection from `peer`, or `None` when it would exceed a limit.
    fn admit(self: &Arc<Self>, peer: SocketAddr) -> Option<Ticket> {
        let mut open = self.open.lock().unwrap();
        let key = peer_key(peer.ip());
        let from_peer = open.per_peer.get(&key).copied().unwrap_or(0);
        if open.pending.len() >= self.limits.total
            || from_peer >= self.limits.per_peer
            || open.pending.contains_key(&peer)
        {
            return None;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        open.pending.insert(peer, id);
        open.per_peer.insert(key, from_peer + 1);
        Some(Ticket {
            connections: self.clone(),
            peer,
            id,
        })
    }
    /// The connection from `peer` is an authenticated device's socket now: it stops counting
    /// and its deadlines no longer apply.
    pub fn authenticated(&self, peer: SocketAddr) {
        self.release(peer, None);
    }
    fn pending(&self, peer: SocketAddr, id: u64) -> bool {
        self.open.lock().unwrap().pending.get(&peer) == Some(&id)
    }
    fn release(&self, peer: SocketAddr, id: Option<u64>) {
        let mut open = self.open.lock().unwrap();
        if open
            .pending
            .get(&peer)
            .is_none_or(|current| id.is_some_and(|id| id != *current))
        {
            return;
        }
        open.pending.remove(&peer);
        let key = peer_key(peer.ip());
        if let Some(count) = open.per_peer.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                open.per_peer.remove(&key);
            }
        }
    }
    #[cfg(test)]
    fn counted(&self) -> usize {
        self.open.lock().unwrap().pending.len()
    }
}
/// One admitted connection; it stops counting when dropped.
struct Ticket {
    connections: Arc<Connections>,
    peer: SocketAddr,
    id: u64,
}
impl Ticket {
    fn pending(&self) -> bool {
        self.connections.pending(self.peer, self.id)
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.connections.release(self.peer, Some(self.id));
    }
}

/// An acceptor that admits a TCP connection against [`Connections`] before `inner` (TLS)
/// runs, and bounds the stream it produces until it is authenticated.
#[derive(Clone)]
pub struct Guarded<A> {
    inner: A,
    connections: Arc<Connections>,
}
impl<A> Guarded<A> {
    pub fn new(inner: A, connections: Arc<Connections>) -> Self {
        Self { inner, connections }
    }
}
impl<A, S> Accept<TcpStream, S> for Guarded<A>
where
    A: Accept<TcpStream, S>,
    A::Future: Send + 'static,
    A::Stream: Send + 'static,
    A::Service: Send + 'static,
{
    type Stream = Bounded<A::Stream>;
    type Service = A::Service;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;
    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let Some(ticket) = stream
            .peer_addr()
            .ok()
            .and_then(|peer| self.connections.admit(peer))
        else {
            // Dropping the stream closes it at once.
            return Box::pin(std::future::ready(Err(io::Error::other(
                "Too many unauthenticated connections",
            ))));
        };
        let limits = self.connections.limits;
        let accepted = self.inner.accept(stream, service);
        Box::pin(async move {
            let (stream, service) = accepted.await?;
            Ok((Bounded::new(stream, ticket, limits), service))
        })
    }
}

/// A connection's stream that fails reading once a deadline passes while it is still
/// unauthenticated: no request byte by `first_byte`, or `lifetime` reached.
pub struct Bounded<S> {
    inner: S,
    ticket: Ticket,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    end: tokio::time::Instant,
    started: bool,
}
impl<S> Bounded<S> {
    fn new(inner: S, ticket: Ticket, limits: Limits) -> Self {
        let now = tokio::time::Instant::now();
        Self {
            inner,
            ticket,
            deadline: Some(Box::pin(tokio::time::sleep_until(
                now + limits.first_byte.min(limits.lifetime),
            ))),
            end: now + limits.lifetime,
            started: false,
        }
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for Bounded<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        while let Some(deadline) = this.deadline.as_mut() {
            if deadline.as_mut().poll(cx).is_pending() {
                break;
            }
            if !this.ticket.pending() {
                this.deadline = None;
            } else if !this.started || tokio::time::Instant::now() >= this.end {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Unauthenticated connection timed out",
                )));
            } else {
                deadline.as_mut().reset(this.end);
            }
        }
        let before = buf.filled().len();
        let read = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(read, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.started = true;
        }
        read
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Bounded<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// A fixed-window budget per peer under a global ceiling.
pub struct RateLimit {
    window: Duration,
    per_peer: u32,
    total: u32,
    spent: Mutex<Spent>,
}
struct Spent {
    since: Instant,
    total: u32,
    peers: HashMap<IpAddr, u32>,
}
impl RateLimit {
    pub fn new(window: Duration, per_peer: u32, total: u32) -> Self {
        Self {
            window,
            per_peer,
            total,
            spent: Mutex::new(Spent {
                since: Instant::now(),
                total: 0,
                peers: HashMap::new(),
            }),
        }
    }
    fn current(&self) -> std::sync::MutexGuard<'_, Spent> {
        let mut spent = self.spent.lock().unwrap();
        if spent.since.elapsed() >= self.window {
            spent.since = Instant::now();
            spent.total = 0;
            spent.peers.clear();
        }
        spent
    }
    /// Spend one from `peer`'s budget and the global one, or `false` when either is used up.
    /// A peer refused for its own budget spends nothing of the global one.
    pub fn take(&self, peer: IpAddr) -> bool {
        let mut spent = self.current();
        let key = peer_key(peer);
        let from_peer = spent.peers.get(&key).copied().unwrap_or(0);
        if from_peer >= self.per_peer || spent.total >= self.total {
            return false;
        }
        spent.total += 1;
        spent.peers.insert(key, from_peer + 1);
        true
    }
    /// Whether `peer` has used up its own budget (the global one is not consulted).
    pub fn exhausted(&self, peer: IpAddr) -> bool {
        self.current()
            .peers
            .get(&peer_key(peer))
            .is_some_and(|spent| *spent >= self.per_peer)
    }
    /// Record one against `peer` without refusing anything; past the global ceiling, peers
    /// not seen in this window go unrecorded, which bounds the memory it takes.
    pub fn spend(&self, peer: IpAddr) {
        let mut spent = self.current();
        let key = peer_key(peer);
        if spent.total >= self.total && !spent.peers.contains_key(&key) {
            return;
        }
        spent.total = spent.total.saturating_add(1);
        *spent.peers.entry(key).or_default() += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    fn peer(address: &str) -> SocketAddr {
        address.parse().unwrap()
    }
    const SHORT: Limits = Limits {
        total: 3,
        per_peer: 2,
        first_byte: Duration::from_secs(10),
        lifetime: Duration::from_secs(120),
        header_read: Duration::from_secs(10),
    };
    #[test]
    fn one_peer_cannot_take_every_unauthenticated_slot() {
        let connections = Connections::new(SHORT);
        let a = connections.admit(peer("192.0.2.1:1000")).unwrap();
        let _b = connections.admit(peer("192.0.2.1:1001")).unwrap();
        assert!(connections.admit(peer("192.0.2.1:1002")).is_none());
        // One IPv6 /64 is one peer; IPv4-mapped addresses are their IPv4 peer.
        let _c = connections.admit(peer("[2001:db8::1]:1000")).unwrap();
        assert!(connections.admit(peer("[::ffff:192.0.2.1]:1003")).is_none());
        assert!(
            connections.admit(peer("198.51.100.1:1000")).is_none(),
            "global"
        );
        drop(a);
        assert!(connections.admit(peer("192.0.2.1:1002")).is_some());
        let connections = Connections::new(SHORT);
        let a = connections.admit(peer("192.0.2.1:1000")).unwrap();
        let _b = connections.admit(peer("[2001:db8::1]:1000")).unwrap();
        let _c = connections.admit(peer("[2001:db8::2]:1000")).unwrap();
        connections.authenticated(peer("192.0.2.1:9"));
        assert!(connections.admit(peer("[2001:db8::3]:1000")).is_none());
        // An authenticated socket stops counting; its later drop releases nothing twice.
        connections.authenticated(a.peer);
        assert_eq!(connections.counted(), 2);
        let _d = connections.admit(peer("192.0.2.1:1000")).unwrap();
        drop(a);
        assert_eq!(connections.counted(), 3);
    }
    #[tokio::test(start_paused = true)]
    async fn silent_and_lingering_unauthenticated_connections_are_closed() {
        let connections = Connections::new(SHORT);
        // Nothing sent: closed at the first-byte deadline.
        let start = tokio::time::Instant::now();
        let (client, server) = tokio::io::duplex(64);
        let ticket = connections.admit(peer("192.0.2.1:1")).unwrap();
        let mut bounded = Bounded::new(server, ticket, SHORT);
        let error = bounded.read(&mut [0; 8]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() >= SHORT.first_byte && start.elapsed() < SHORT.lifetime);
        drop((bounded, client));
        assert_eq!(connections.counted(), 0);
        // A busy connection that never authenticates lasts its lifetime.
        let start = tokio::time::Instant::now();
        let (mut client, server) = tokio::io::duplex(64);
        let ticket = connections.admit(peer("192.0.2.1:2")).unwrap();
        let mut bounded = Bounded::new(server, ticket, SHORT);
        client.write_all(b"G").await.unwrap();
        assert_eq!(bounded.read(&mut [0; 8]).await.unwrap(), 1);
        let error = bounded.read(&mut [0; 8]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() >= SHORT.lifetime);
        // An authenticated one outlives both deadlines.
        let (mut client, server) = tokio::io::duplex(64);
        let ticket = connections.admit(peer("192.0.2.1:3")).unwrap();
        let mut bounded = Bounded::new(server, ticket, SHORT);
        connections.authenticated(peer("192.0.2.1:3"));
        let reader = tokio::spawn(async move {
            let mut byte = [0; 1];
            bounded.read_exact(&mut byte).await.map(|_| byte[0])
        });
        tokio::time::sleep(SHORT.lifetime * 2).await;
        client.write_all(b"x").await.unwrap();
        assert_eq!(reader.await.unwrap().unwrap(), b'x');
    }
    #[test]
    fn a_peer_over_its_budget_leaves_others_theirs() {
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        let b: IpAddr = "198.51.100.1".parse().unwrap();
        let limit = RateLimit::new(Duration::from_secs(60), 2, 3);
        assert!(limit.take(a) && limit.take(a));
        assert!(!limit.take(a) && !limit.take(a));
        assert!(
            limit.take(b),
            "a refused peer spends nothing of the global budget"
        );
        assert!(
            !limit.take("203.0.113.1".parse().unwrap()),
            "global ceiling"
        );
        let failures = RateLimit::new(Duration::from_secs(60), 2, 3);
        failures.spend(a);
        assert!(!failures.exhausted(a));
        failures.spend(a);
        assert!(failures.exhausted(a) && !failures.exhausted(b));
        let expired = RateLimit::new(Duration::ZERO, 1, 1);
        assert!(expired.take(a) && expired.take(a));
    }
}
