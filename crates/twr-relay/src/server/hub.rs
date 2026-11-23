//! Hub sessions (docs/relay-protocol.md §5).

use super::counters::Refusal;
use super::stores::MovedNote;
use super::{now, Hub, Session, Shared};
use crate::mux::{self, MuxStream};
use crate::proto::{self, Incoming, Msg, PhoneRoute, PortRoute, Welcome};
use crate::{keys, names};
use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tokio_util::sync::CancellationToken;

const SILENT_LIMIT: Duration = Duration::from_secs(90);

fn random_hex(n: usize) -> String {
    use ring::rand::SecureRandom;
    let mut b = vec![0u8; n];
    ring::rand::SystemRandom::new().fill(&mut b).expect("randomness");
    hex::encode(b)
}

async fn send_fatal(ctl: &mut MuxStream, code: &str, message: &str) {
    use tokio::io::AsyncWriteExt as _;
    let _ = proto::write_frame(ctl, &Msg::error(code, message, true)).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), ctl.shutdown()).await;
}

pub async fn serve(shared: Arc<Shared>, tls: TlsStream<TcpStream>, peer: SocketAddr) {
    let conn = tls.get_ref().1;
    if conn.protocol_version() != Some(rustls::ProtocolVersion::TLSv1_3) {
        shared.counters.refuse(Refusal::HubAuth);
        return;
    }
    let Ok(exporter) = conn.export_keying_material([0u8; 32], proto::EXPORTER_LABEL, Some(proto::ALPN_V1)) else {
        return;
    };

    let cancel = shared.cancel.child_token();
    let (open_tx, open_rx) = mpsc::channel::<mux::OpenRequest>(16);
    let (in_tx, mut in_rx) = mpsc::channel::<MuxStream>(4);
    let dcancel = cancel.clone();
    tokio::spawn(async move {
        let _ = mux::drive(tls, mux::relay_config(), yamux::Mode::Server, open_rx, in_tx, dcancel.clone()).await;
        dcancel.cancel();
    });
    // On any return, close the connection gracefully: the driver flushes what is queued (a
    // fatal error, say) before it closes, within its own timeout.
    let _close = CancelOnDrop(cancel.clone());

    // The control stream and hello, within the hello timeout (§5.3).
    let deadline = Duration::from_secs(shared.cfg.limits.hub_hello_seconds);
    let first = tokio::time::timeout(deadline, async {
        let mut ctl = in_rx.recv().await?;
        let frame = proto::read_frame(&mut ctl).await.ok().flatten();
        Some((ctl, frame))
    })
    .await;
    let Ok(Some((mut ctl, frame))) = first else {
        shared.counters.refuse(Refusal::HubAuth);
        return;
    };
    let (spki_b64, sig_b64) = match frame {
        Some(Incoming::Msg(Msg::Hello { spki, sig, .. })) => (spki, sig),
        _ => {
            send_fatal(&mut ctl, "hello_required", "send hello first").await;
            shared.counters.refuse(Refusal::HubAuth);
            return;
        }
    };
    let (Some(spki), Some(sig)) = (names::unb64(&spki_b64), names::unb64(&sig_b64)) else {
        send_fatal(&mut ctl, "bad_request", "spki and sig are base64url").await;
        shared.counters.refuse(Refusal::HubAuth);
        return;
    };
    if keys::parse_spki(&spki).is_none() {
        send_fatal(&mut ctl, "bad_key", "P-256 or Ed25519 keys only").await;
        shared.counters.refuse(Refusal::HubAuth);
        return;
    }
    if !keys::verify(&spki, &proto::hub_auth_message(&exporter), &sig) {
        send_fatal(&mut ctl, "bad_signature", "the hello signature does not verify").await;
        shared.counters.refuse(Refusal::HubAuth);
        return;
    }
    let name = names::hub_name(&names::pin(&spki));
    if shared.deny.hubs.contains(&name) {
        send_fatal(&mut ctl, "denied", "").await;
        shared.counters.refuse(Refusal::Denied);
        return;
    }
    // New hub names per address per hour (§11). A name seen before from here doesn't count.
    let over = {
        let mut seen = shared.limits.hub_names.lock().unwrap();
        let list = seen.entry(super::limits::IpKey::of(peer.ip())).or_default();
        list.retain(|(_, at)| at.elapsed() < Duration::from_secs(3600));
        if list.iter().any(|(n, _)| n == &name) {
            false
        } else if list.len() >= shared.cfg.limits.new_hubs_per_ip_per_hour as usize {
            true
        } else {
            list.push((name.clone(), Instant::now()));
            false
        }
    };
    if over {
        send_fatal(&mut ctl, "rate_limited", "too many new hubs from this address").await;
        shared.counters.refuse(Refusal::HubRate);
        return;
    }

    let hub = {
        let mut hubs = shared.hubs.lock().unwrap();
        if !hubs.contains_key(&name) && hubs.len() >= shared.cfg.limits.max_hubs as usize {
            None
        } else {
            Some(hubs.entry(name.clone()).or_insert_with(|| Arc::new(Hub::new(name.clone()))).clone())
        }
    };
    let Some(hub) = hub else {
        send_fatal(&mut ctl, "capacity", "the relay is full").await;
        shared.counters.refuse(Refusal::Capacity);
        return;
    };

    // Writer for the control stream.
    let (mut ctl_r, mut ctl_w) = tokio::io::split(ctl);
    let (out_tx, mut out_rx) = mpsc::channel::<Msg>(32);
    let wcancel = cancel.clone();
    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                _ = wcancel.cancelled() => {
                    // Flush what is queued (a drain or a fatal error), then stop.
                    while let Ok(m) = out_rx.try_recv() {
                        let _ = proto::write_frame(&mut ctl_w, &m).await;
                    }
                    break;
                }
                m = out_rx.recv() => match m { Some(m) => m, None => break },
            };
            let fatal = matches!(msg, Msg::Error { fatal: true, .. });
            if proto::write_frame(&mut ctl_w, &msg).await.is_err() || fatal {
                wcancel.cancel();
                break;
            }
        }
    });
    let _writer_guard = AbortOnDrop(writer);

    let session_id = random_hex(16);
    let replaced = hub.session.lock().unwrap().replace(Session {
        id: session_id.clone(),
        open: open_tx,
        out: out_tx.clone(),
        cancel: cancel.clone(),
    });
    if let Some(old) = replaced {
        let _ = old.out.try_send(Msg::error("replaced", "a newer session for this hub took over", true));
        let c = old.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.cancel();
        });
    } else {
        shared.counters.hubs_connected.fetch_add(1, Relaxed);
    }
    *hub.offline_since.lock().unwrap() = 0;
    shared.counters.hub_sessions_total.fetch_add(1, Relaxed);

    let welcome = Welcome {
        hub: name.clone(),
        session: session_id.clone(),
        server: format!("twr {}", shared.version),
        region: shared.cfg.relay.region.clone(),
        features: vec![],
        phone_routes: shared
            .suffixes
            .iter()
            .map(|s| PhoneRoute {
                host: shared.public_host.clone(),
                port: shared.public_port,
                suffix: s.clone(),
                tags: names::TAG_VERSIONS.to_vec(),
            })
            .collect(),
        port_route: shared.port_routes.get(&name).map(|p| PortRoute { host: shared.public_host.clone(), port: *p }),
        relay_pin: shared.relay_pin_text(),
        iroh_url: shared.iroh_url.clone(),
        records_url: Some(shared.records_url()),
        limits: shared.limits_msg(),
    };
    let _ = out_tx.send(Msg::Welcome(welcome)).await;

    // Further streams from the hub aren't part of version 1: reset them.
    let icancel = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = icancel.cancelled() => return,
                s = in_rx.recv() => if s.is_none() { return },
            }
        }
    });

    control_loop(&shared, &hub, &spki, &mut ctl_r, &out_tx, &cancel).await;

    cancel.cancel();
    let ours = {
        let mut s = hub.session.lock().unwrap();
        if s.as_ref().is_some_and(|s| s.id == session_id) {
            *s = None;
            true
        } else {
            false
        }
    };
    if ours {
        shared.counters.hubs_connected.fetch_sub(1, Relaxed);
        *hub.offline_since.lock().unwrap() = now();
    }
}

async fn control_loop<R: tokio::io::AsyncRead + Unpin>(
    shared: &Shared,
    hub: &Hub,
    spki: &[u8],
    ctl: &mut R,
    out: &mpsc::Sender<Msg>,
    cancel: &CancellationToken,
) {
    loop {
        let frame = tokio::select! {
            _ = cancel.cancelled() => return,
            f = tokio::time::timeout(SILENT_LIMIT, proto::read_frame(ctl)) => f,
        };
        let msg = match frame {
            Err(_) | Ok(Err(_)) | Ok(Ok(None)) => return,
            Ok(Ok(Some(Incoming::Unknown(t)))) => {
                let _ = out.send(Msg::Error { code: "unsupported".into(), message: String::new(), fatal: false, ref_: Some(t) }).await;
                continue;
            }
            Ok(Ok(Some(Incoming::Msg(m)))) => m,
        };
        let reply = match msg {
            Msg::Ping { n } => Msg::Pong { n },
            Msg::Pong { .. } => continue,
            Msg::Phones { phones } => set_phones(shared, hub, phones),
            Msg::Moved { body, sig } => moved(shared, hub, spki, body, sig),
            Msg::Error { fatal: true, .. } => return,
            Msg::Error { .. } => continue,
            other => Msg::Error {
                code: "bad_request".into(),
                message: "not a hub message".into(),
                fatal: false,
                ref_: Some(type_of(&other).into()),
            },
        };
        if out.send(reply).await.is_err() {
            return;
        }
    }
}

fn type_of(m: &Msg) -> &'static str {
    match m {
        Msg::Hello { .. } => "hello",
        Msg::Welcome(_) => "welcome",
        Msg::Phones { .. } => "phones",
        Msg::PhonesOk { .. } => "phones_ok",
        Msg::Moved { .. } => "moved",
        Msg::MovedOk {} => "moved_ok",
        Msg::Ping { .. } => "ping",
        Msg::Pong { .. } => "pong",
        Msg::Drain { .. } => "drain",
        Msg::Error { .. } => "error",
        Msg::Phone { .. } => "phone",
    }
}

fn err(code: &str, message: &str, of: &str) -> Msg {
    Msg::Error { code: code.into(), message: message.into(), fatal: false, ref_: Some(of.into()) }
}

fn set_phones(shared: &Shared, hub: &Hub, phones: Vec<proto::PhoneCred>) -> Msg {
    if phones.len() > shared.cfg.limits.phones_per_hub as usize {
        return err("too_many_phones", "over this relay's limit", "phones");
    }
    let mut creds = Vec::with_capacity(phones.len());
    for p in phones {
        let id_ok = p.id.len() == 32 && p.id.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
        let key: Option<[u8; 32]> = names::unb64(&p.key).and_then(|k| k.try_into().ok());
        match (id_ok, key) {
            (true, Some(k)) => {
                if creds.iter().any(|(id, _): &(String, [u8; 32])| id == &p.id) {
                    return err("bad_request", "duplicate id", "phones");
                }
                creds.push((p.id, k));
            }
            _ => return err("bad_request", "id is 32 hex characters, key 32 bytes base64url", "phones"),
        }
    }
    let count = creds.len() as u32;
    let mut set = hub.phones.lock().unwrap();
    // Cut off connections of credentials that are gone (§5.5).
    let keep: std::collections::HashSet<&String> = creds.iter().map(|(id, _)| id).collect();
    let gone: Vec<String> = set.tokens.keys().filter(|id| !keep.contains(id)).cloned().collect();
    for id in gone {
        if let Some(t) = set.tokens.remove(&id) {
            t.cancel();
        }
    }
    set.creds = creds;
    Msg::PhonesOk { count }
}

fn moved(shared: &Shared, hub: &Hub, spki: &[u8], body_b64: String, sig_b64: String) -> Msg {
    let (Some(body), Some(sig)) = (names::unb64(&body_b64), names::unb64(&sig_b64)) else {
        return err("bad_request", "body and sig are base64url", "moved");
    };
    if body.len() > proto::MAX_MOVED_BODY {
        return err("bad_request", "body over 4096 bytes", "moved");
    }
    if !keys::verify(spki, &proto::moved_message(&body), &sig) {
        return err("bad_signature", "", "moved");
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return err("bad_request", "body is not JSON", "moved");
    };
    let t = now();
    let hub_ok = v.get("hub").and_then(|h| h.as_str()) == Some(hub.name.as_str());
    let issued_ok = v.get("issued").and_then(|i| i.as_u64()).is_some_and(|i| i.abs_diff(t) <= 300);
    if !hub_ok || !issued_ok {
        return err("bad_request", "body.hub must be this hub and body.issued within 5 minutes", "moved");
    }
    shared.state.set_moved(&hub.name, MovedNote { spki: names::b64(spki), body: body_b64, sig: sig_b64, stored: t });
    Msg::MovedOk {}
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
