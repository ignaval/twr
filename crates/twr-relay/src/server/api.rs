//! The HTTPS API (docs/relay-protocol.md §8-§10).

use super::counters::Refusal;
use super::stores::{parse_pkarr_key, PutError};
use super::{now, Shared};
use crate::{admit, names, proto};
use bytes::Bytes;
use http::{header, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

type Resp = Response<Full<Bytes>>;

fn json(status: StatusCode, v: serde_json::Value) -> Resp {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(serde_json::to_vec(&v).unwrap())))
        .unwrap()
}

fn error(status: StatusCode, code: &str) -> Resp {
    json(status, serde_json::json!({ "error": code }))
}

fn rate_limited() -> Resp {
    let mut r = error(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    r.headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
    r
}

pub async fn serve(shared: Arc<Shared>, tls: TlsStream<TcpStream>, peer: SocketAddr) {
    use hyper::service::service_fn;
    let s = shared.clone();
    let svc = service_fn(move |req: Request<Incoming>| {
        let s = s.clone();
        async move { Ok::<_, std::convert::Infallible>(route(&s, req, peer).await) }
    });
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(std::time::Duration::from_secs(10));
    builder.http2().max_concurrent_streams(16);
    let conn = builder.serve_connection(hyper_util::rt::TokioIo::new(tls), svc);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(60), conn).await;
}

async fn body(req: Request<Incoming>, max: usize) -> Option<Bytes> {
    Limited::new(req.into_body(), max)
        .collect()
        .await
        .ok()
        .map(|c| c.to_bytes())
}

async fn route(s: &Shared, req: Request<Incoming>, peer: SocketAddr) -> Resp {
    s.counters.api_requests.fetch_add(1, Relaxed);
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, parts.as_slice()) {
        (Method::GET, ["v1", "info"]) => info(s),
        (Method::GET, ["v1", "hubs", label]) => hub_status(s, label),
        (Method::GET, ["v1", "hubs", hub, "moved"]) => moved(s, hub),
        (Method::POST, ["v1", "iroh", "admit"]) => {
            let Some(b) = body(req, admit::MAX_BODY).await else {
                return error(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
            };
            admission(s, &b, peer)
        }
        (Method::PUT, ["pkarr", key]) => {
            if !s.allow(&s.limits.record_puts, peer.ip()) {
                s.counters.refuse(Refusal::RecordRate);
                return rate_limited();
            }
            let Some(key) = parse_pkarr_key(key) else {
                return error(StatusCode::BAD_REQUEST, "bad_request");
            };
            let Some(b) = body(req, 72 + super::stores::MAX_PACKET).await else {
                return error(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
            };
            match s
                .records
                .put(&key, &b, now(), s.cfg.limits.max_records as usize)
            {
                Ok(()) => {
                    s.counters.record_puts.fetch_add(1, Relaxed);
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Full::new(Bytes::new()))
                        .unwrap()
                }
                Err(e) => {
                    s.counters.refuse(Refusal::RecordRejected);
                    match e {
                        PutError::Bad => error(StatusCode::BAD_REQUEST, "bad_request"),
                        PutError::Older => error(StatusCode::CONFLICT, "not_most_recent"),
                        PutError::SameTimestamp => {
                            error(StatusCode::PRECONDITION_REQUIRED, "not_most_recent")
                        }
                        PutError::Full => error(StatusCode::SERVICE_UNAVAILABLE, "capacity"),
                    }
                }
            }
        }
        (Method::GET, ["pkarr", key]) => {
            let Some(key) = parse_pkarr_key(key) else {
                return error(StatusCode::BAD_REQUEST, "bad_request");
            };
            let Some(rec) = s.records.get(&key, now(), s.cfg.limits.record_ttl_seconds) else {
                return error(StatusCode::NOT_FOUND, "not_found");
            };
            let stored = httpdate(rec.stored_unix);
            if let Some(since) = req
                .headers()
                .get(header::IF_MODIFIED_SINCE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_httpdate)
            {
                if since >= rec.stored_unix {
                    return Response::builder()
                        .status(StatusCode::NOT_MODIFIED)
                        .body(Full::new(Bytes::new()))
                        .unwrap();
                }
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/pkarr.org/relays#payload")
                .header(header::CACHE_CONTROL, "public, max-age=30")
                .header(header::LAST_MODIFIED, &stored)
                .header("memento-datetime", &stored)
                .body(Full::new(rec.payload))
                .unwrap()
        }
        _ => error(StatusCode::NOT_FOUND, "not_found"),
    }
}

fn info(s: &Shared) -> Resp {
    let mut r = json(
        StatusCode::OK,
        serde_json::json!({
            "relay": "twr-relay",
            "server": format!("twr {}", s.version),
            "region": s.cfg.relay.region,
            "hub_protocols": proto::HUB_ALPNS.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect::<Vec<_>>(),
            "tag_versions": names::TAG_VERSIONS,
            "admission_versions": admit::ADMIT_VERSIONS,
            "records": "pkarr",
            "phone_suffixes": s.suffixes,
            "iroh_url": s.iroh_url,
            "records_url": s.records_url(),
            "relay_pin": s.relay_pin_text(),
        }),
    );
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("public, max-age=300"),
    );
    r
}

fn hub_status(s: &Shared, label: &str) -> Resp {
    let unknown = || error(StatusCode::NOT_FOUND, "unknown");
    let Some((hub_name, tag)) = names::parse_label(label) else {
        return unknown();
    };
    let Some(hub) = s.hub(hub_name) else {
        return unknown();
    };
    if hub.match_tag(tag, now()).is_none() {
        return unknown();
    }
    json(
        StatusCode::OK,
        serde_json::json!({ "hub": hub_name, "connected": hub.connected() }),
    )
}

fn moved(s: &Shared, hub: &str) -> Resp {
    if !names::is_hub_name(hub) {
        return error(StatusCode::NOT_FOUND, "none");
    }
    match s.state.moved(hub, now()) {
        Some(n) => json(
            StatusCode::OK,
            serde_json::json!({ "spki": n.spki, "body": n.body, "sig": n.sig }),
        ),
        None => error(StatusCode::NOT_FOUND, "none"),
    }
}

fn admission(s: &Shared, b: &[u8], peer: SocketAddr) -> Resp {
    // Rate-limit by kind before checking signatures, which cost more.
    let pairing = serde_json::from_slice::<serde_json::Value>(b)
        .ok()
        .and_then(|v| {
            v.get("kind")
                .and_then(|k| k.as_str())
                .map(|k| k == "pairing")
        })
        .unwrap_or(true);
    let limiter = if pairing {
        &s.limits.pairing_admits
    } else {
        &s.limits.pair_admits
    };
    if !s.allow(limiter, peer.ip()) {
        s.counters.refuse(Refusal::AdmitRate);
        return rate_limited();
    }
    let t = now();
    match admit::verify(b, t) {
        Err(e) => {
            s.counters.refuse(Refusal::AdmitRejected);
            error(StatusCode::BAD_REQUEST, e.code())
        }
        Ok(a) => {
            if a.endpoints.iter().any(|e| s.deny.endpoints.contains(e)) {
                s.counters.refuse(Refusal::Denied);
                return error(StatusCode::FORBIDDEN, "denied");
            }
            if s.state
                .admit(&a.endpoints, a.until, s.cfg.limits.max_admitted as usize)
                .is_err()
            {
                return error(StatusCode::SERVICE_UNAVAILABLE, "capacity");
            }
            s.counters
                .iroh_admitted
                .store(s.state.admitted_count() as i64, Relaxed);
            json(
                StatusCode::OK,
                serde_json::json!({
                    "admitted": a.endpoints.iter().map(hex::encode).collect::<Vec<_>>(),
                    "until": a.until,
                }),
            )
        }
    }
}

// HTTP dates (RFC 9110 IMF-fixdate), for the record store's Last-Modified.

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub fn httpdate(unix: u64) -> String {
    let days = (unix / 86400) as i64;
    let secs = unix % 86400;
    let (y, m, d) = civil(days);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

pub fn parse_httpdate(text: &str) -> Option<u64> {
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let rest = text.split_once(", ")?.1;
    let mut it = rest.split(' ');
    let d: u32 = it.next()?.parse().ok()?;
    let month = it.next()?;
    let m = MONTHS.iter().position(|x| *x == month)? as u32 + 1;
    let y: i64 = it.next()?.parse().ok()?;
    let hms: Vec<u64> = it
        .next()?
        .split(':')
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    if it.next()? != "GMT" || hms.len() != 3 || y < 1970 {
        return None;
    }
    // Days from civil, as in der.rs.
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days as u64 * 86400 + hms[0] * 3600 + hms[1] * 60 + hms[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dates() {
        assert_eq!(httpdate(784111777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(
            parse_httpdate("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784111777)
        );
        for t in [0u64, 951782400, 1_800_000_000, 4_102_444_800] {
            assert_eq!(parse_httpdate(&httpdate(t)), Some(t));
        }
        assert_eq!(parse_httpdate("garbage"), None);
    }
}
