use std::{
    collections::HashMap,
    net::{Ipv4Addr, TcpListener},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    routing::put,
    Router,
};
use pkarr::{Client, Keypair, PublicKey, ResolvePolicy, SignedPacket};
use twr_runner_link_spike::locator::{Record, MAX_RELAY_PAYLOAD};
use twr_runner_link_spike::pairing::Code;

type Packets = Arc<Mutex<HashMap<PublicKey, SignedPacket>>>;
const NOW: u64 = 1_000_000;

fn record() -> Record {
    Record {
        version: 1,
        endpoint: iroh::SecretKey::generate().public().to_string(),
        relay: "http://127.0.0.1:4545".into(),
        expires_unix: NOW + 900,
        session: hex::encode([1; 16]),
        admission: hex::encode([2; 16]),
    }
}

async fn store(
    State(packets): State<Packets>,
    Path(key): Path<String>,
    bytes: Bytes,
) -> StatusCode {
    let Ok(key) = key.parse::<PublicKey>() else {
        return StatusCode::BAD_REQUEST;
    };
    let Ok(packet) = SignedPacket::from_relay_payload(&key, &bytes) else {
        return StatusCode::BAD_REQUEST;
    };
    let mut packets = packets.lock().unwrap();
    if packets
        .get(&key)
        .is_some_and(|old| old.timestamp() >= packet.timestamp())
    {
        return StatusCode::CONFLICT;
    }
    packets.insert(key, packet);
    StatusCode::NO_CONTENT
}

async fn load(
    State(packets): State<Packets>,
    Path(key): Path<String>,
) -> Result<Bytes, StatusCode> {
    let key = key
        .parse::<PublicKey>()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    packets
        .lock()
        .unwrap()
        .get(&key)
        .map(SignedPacket::to_relay_payload)
        .ok_or(StatusCode::NOT_FOUND)
}

#[test]
fn signed_record_is_bounded_expiring_and_not_reusable_under_another_key() -> Result<()> {
    let key = Keypair::random();
    let record = record();
    let packet = record.sign(&key)?;
    assert!(packet.to_relay_payload().len() <= MAX_RELAY_PAYLOAD);
    let parsed = Record::parse(&packet, &key.public_key(), NOW)?;
    assert_eq!(parsed.address()?.relay_urls().count(), 1);
    assert_eq!(parsed.address()?.ip_addrs().count(), 0);
    assert!(Record::parse(&packet, &key.public_key(), NOW + 900).is_err());
    assert!(Record::parse(&packet, &Keypair::random().public_key(), NOW).is_err());
    let mut corrupt = packet.to_relay_payload().to_vec();
    corrupt[0] ^= 1;
    assert!(SignedPacket::from_relay_payload(&key.public_key(), &corrupt.into()).is_err());
    Ok(())
}

#[test]
fn code_derived_key_signs_and_resolves_the_pairing_record() -> Result<()> {
    let showing = Code::parse("4HK7-M2QP-9XTR")?.derive()?;
    let typing = Code::parse("4hk7 m2qp 9xtr")?.derive()?;
    let packet = record().sign(&showing.locator_key())?;
    let parsed = Record::parse(&packet, &typing.locator_key().public_key(), NOW)?;
    assert_eq!(parsed.session, hex::encode([1; 16]));
    Ok(())
}

#[tokio::test]
async fn local_http_publication_and_tombstone_with_no_dht() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let url = format!("http://{}/pkarr", listener.local_addr()?);
        let packets = Packets::default();
        let app = Router::new()
            .route("/pkarr/{key}", put(store).get(load))
            .layer(DefaultBodyLimit::max(MAX_RELAY_PAYLOAD))
            .with_state(packets);
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let mut builder = Client::builder();
        builder.no_default_network().relays(&[url])?.cache_size(0);
        let publisher = builder.build()?;
        let receiver = builder.build()?;
        let key = Keypair::random();
        let packet = record().sign(&key)?;
        publisher.publish(&packet).await?;
        let fetched = receiver
            .resolve(&key.public_key(), ResolvePolicy::CacheFirst)
            .await?;
        Record::parse(&fetched, &key.public_key(), NOW)?;
        let tombstone = SignedPacket::builder()
            .timestamp((packet.timestamp().as_u64() + 1).into())
            .sign(&key)?;
        publisher.publish(&tombstone).await?;
        let fetched = receiver
            .resolve(&key.public_key(), ResolvePolicy::CacheFirst)
            .await?;
        assert!(Record::parse(&fetched, &key.public_key(), NOW).is_err());
        server.abort();
        let _ = server.await;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_cache_dht_resolves_complete_address_when_http_is_down() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let testnet = mainline::Testnet::builder(3).build()?;
        // A listening socket that never accepts gives no HTTP response (200 ms timeout).
        let unused = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let unavailable = format!("http://{}/pkarr", unused.local_addr()?);
        let mut builder = Client::builder();
        builder
            .no_default_network()
            .bootstrap(&testnet.bootstrap)
            .dht(|config| {
                config.bind_address = Some(Ipv4Addr::LOCALHOST);
                config
            })
            .dht_report_policy(pkarr::dht::ReportPolicy::testnet())
            .relays(&[unavailable])?
            .relay_request_timeout(Duration::from_millis(200))
            .cache_size(0);
        let publisher = builder.build()?;
        let receiver = builder.build()?;
        let key = Keypair::random();
        publisher.publish(&record().sign(&key)?).await?;
        let fetched = receiver
            .resolve(&key.public_key(), ResolvePolicy::NetworkOnly)
            .await?;
        let parsed = Record::parse(&fetched, &key.public_key(), NOW)?;
        assert_eq!(
            parsed.address()?.relay_urls().next().unwrap().to_string(),
            "http://127.0.0.1:4545/"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}
