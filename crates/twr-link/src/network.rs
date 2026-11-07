use crate::{ALPN, PKARR_RELAY};
use anyhow::Result;
use iroh::{Endpoint, SecretKey};

/// No endpoint is constructed until an explicit pairing window or a saved link needs it.
/// Only relay addresses are published. DNS and DHT both resolve ordinary reconnects.
pub async fn endpoint(secret: [u8; 32]) -> Result<Endpoint> {
    use iroh::{
        address_lookup::{DnsAddressLookup, PkarrPublisher, PkarrResolver},
        endpoint::presets,
    };
    let key = SecretKey::from_bytes(&secret);
    Ok(Endpoint::builder(presets::Minimal)
        .secret_key(key.clone())
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(iroh::RelayMode::Default)
        .address_lookup(PkarrPublisher::builder(PKARR_RELAY.parse()?))
        .address_lookup(PkarrResolver::builder(PKARR_RELAY.parse()?))
        .address_lookup(DnsAddressLookup::n0_dns())
        .address_lookup(iroh_mainline_address_lookup::DhtAddressLookup::builder().secret_key(key))
        .bind()
        .await?)
}
pub fn locator_client() -> Result<pkarr::Client> {
    let mut builder = pkarr::Client::builder();
    builder
        .no_default_network()
        .dht(|c| c)
        .relays(&[PKARR_RELAY])?;
    Ok(builder.build()?)
}
pub fn exporter(connection: &iroh::endpoint::Connection) -> Result<[u8; 32]> {
    let mut key = [0; 32];
    connection
        .export_keying_material(&mut key, b"EXPORTER-twr-pair-v1", crate::ALPN)
        .map_err(|_| anyhow::anyhow!("QUIC exporter unavailable"))?;
    Ok(key)
}

pub async fn publish(packet: pkarr::SignedPacket) -> Result<()> {
    locator_client()?.publish(&packet).await?;
    Ok(())
}
