//! Phone passthrough (docs/relay-protocol.md §6, §7): check, open a stream to the hub, copy.

use super::counters::Refusal;
use super::limits::Bucket;
use super::{now, Shared};
use crate::proto::{self, Msg};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

struct ActiveGuard<'a>(
    &'a std::sync::atomic::AtomicU32,
    &'a super::counters::Counters,
);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
        self.1.phone_active.fetch_sub(1, Relaxed);
    }
}

pub async fn serve(
    shared: Arc<Shared>,
    tcp: TcpStream,
    peer: SocketAddr,
    hub_name: String,
    tag: Option<String>,
    mode: &str,
) {
    if !shared.allow(&shared.limits.phone_attempts, peer.ip()) {
        shared.counters.refuse(Refusal::PhoneRate);
        return;
    }
    let Some(hub) = shared.hub(&hub_name) else {
        shared.counters.refuse(Refusal::HubOffline);
        return;
    };
    let t = now();
    let phone_id = match &tag {
        Some(tag) => match hub.match_tag(tag, t) {
            Some(id) => Some(id),
            None => {
                shared.counters.refuse(Refusal::BadTag);
                return;
            }
        },
        None => None,
    };
    let Some((open, session_cancel)) = hub
        .session
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| (s.open.clone(), s.cancel.clone()))
    else {
        shared.counters.refuse(Refusal::HubOffline);
        return;
    };
    let cap = shared.cfg.limits.phone_connections_per_hub;
    if hub.active.fetch_add(1, Relaxed) >= cap {
        hub.active.fetch_sub(1, Relaxed);
        shared.counters.refuse(Refusal::HubBusy);
        return;
    }
    shared.counters.phone_active.fetch_add(1, Relaxed);
    let _active = ActiveGuard(&hub.active, &shared.counters);
    if shared.state.usage(&hub_name, t) >= shared.cfg.limits.monthly_bytes_per_hub {
        shared.counters.refuse(Refusal::Quota);
        return;
    }
    let phone_cancel = match &phone_id {
        Some(id) => {
            let mut set = hub.phones.lock().unwrap();
            set.tokens.entry(id.clone()).or_default().child_token()
        }
        None => session_cancel.child_token(),
    };

    let (tx, rx) = oneshot::channel();
    if open.send(tx).await.is_err() {
        shared.counters.refuse(Refusal::HubOffline);
        return;
    }
    let mut stream = match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(Ok(s))) => s,
        _ => {
            shared.counters.refuse(Refusal::HubOffline);
            return;
        }
    };
    let header = Msg::Phone {
        phone: phone_id,
        mode: mode.into(),
        remote: peer.to_string(),
    };
    if proto::write_frame(&mut stream, &header).await.is_err() {
        return;
    }
    shared.counters.phone_total.fetch_add(1, Relaxed);

    let to_hub = Arc::new(AtomicU64::new(0));
    let to_phone = Arc::new(AtomicU64::new(0));
    let limits = &shared.cfg.limits;
    let (rate, burst) = (limits.bytes_per_second as f64, limits.burst_bytes as f64);
    let idle = Duration::from_secs(limits.idle_seconds);
    let last = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let (pr, pw) = tcp.into_split();
    let (hr, hw) = tokio::io::split(stream);
    let up = pump(
        pr,
        hw,
        Bucket::new(rate, burst),
        to_hub.clone(),
        last.clone(),
        start,
    );
    let down = pump(
        hr,
        pw,
        Bucket::new(rate, burst),
        to_phone.clone(),
        last.clone(),
        start,
    );
    let both = async { tokio::try_join!(up, down) };
    tokio::pin!(both);
    let mut flushed = (0u64, 0u64);
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.tick().await;
    loop {
        tokio::select! {
            _ = &mut both => break,
            _ = phone_cancel.cancelled() => break,
            _ = session_cancel.cancelled() => break,
            _ = tick.tick() => {
                flush(&shared, &hub_name, &to_hub, &to_phone, &mut flushed);
                let quiet = start.elapsed().saturating_sub(Duration::from_millis(last.load(Relaxed)));
                if quiet >= idle {
                    break;
                }
            }
        }
    }
    flush(&shared, &hub_name, &to_hub, &to_phone, &mut flushed);
}

fn flush(
    shared: &Shared,
    hub: &str,
    to_hub: &AtomicU64,
    to_phone: &AtomicU64,
    done: &mut (u64, u64),
) {
    let (a, b) = (to_hub.load(Relaxed), to_phone.load(Relaxed));
    let (da, db) = (a - done.0, b - done.1);
    shared.counters.bytes_to_hub.fetch_add(da, Relaxed);
    shared.counters.bytes_to_phone.fetch_add(db, Relaxed);
    shared.state.add_usage(hub, now(), da + db);
    *done = (a, b);
}

/// Copies one direction, at most `bucket`'s rate, recording activity for the idle timeout.
async fn pump<R, W>(
    mut r: R,
    mut w: W,
    mut bucket: Bucket,
    count: Arc<AtomicU64>,
    last: Arc<AtomicU64>,
    start: Instant,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            let _ = w.shutdown().await;
            return Ok(());
        }
        last.store(start.elapsed().as_millis() as u64, Relaxed);
        let wait = bucket.take(n as f64);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        w.write_all(&buf[..n]).await?;
        w.flush().await?;
        count.fetch_add(n as u64, Relaxed);
    }
}
