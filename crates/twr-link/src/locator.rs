//! Code-derived signed records. Parsing a record never grants runner authority.

use anyhow::{ensure, Result};
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use pkarr::{dns::rdata::RData, Keypair, PublicKey, SignedPacket};
use serde::{Deserialize, Serialize};

pub const MAX_RELAY_PAYLOAD: usize = 1072; // 1000-byte DNS packet + signature/timestamp

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub version: u8,
    pub endpoint: String,
    pub relay: String,
    pub expires_unix: u64,
    pub session: String,
    pub admission: String,
}

impl Record {
    pub fn sign(&self, key: &Keypair) -> Result<SignedPacket> {
        let json = serde_json::to_string(self)?;
        ensure!(json.len() <= 700, "pairing record too large");
        let packet = SignedPacket::builder()
            .txt("_twr-pair".try_into()?, json.as_str().try_into()?, 900)
            .sign(key)?;
        ensure!(
            packet.to_relay_payload().len() <= MAX_RELAY_PAYLOAD,
            "packet too large"
        );
        Ok(packet)
    }

    pub fn parse(packet: &SignedPacket, expected: &PublicKey, now: u64) -> Result<Self> {
        ensure!(&packet.public_key() == expected, "wrong locator key");
        ensure!(
            packet.to_relay_payload().len() <= MAX_RELAY_PAYLOAD,
            "packet too large"
        );
        let mut records = packet.resource_records("_twr-pair");
        let record = records
            .next()
            .ok_or_else(|| anyhow::anyhow!("pairing record absent"))?;
        ensure!(records.next().is_none(), "ambiguous pairing record");
        let RData::TXT(text) = &record.rdata else {
            anyhow::bail!("not a pairing TXT record")
        };
        let json = String::try_from(text.clone())?;
        ensure!(json.len() <= 700, "record too large");
        let value: Self = serde_json::from_str(&json)?;
        ensure!(value.version == 1, "unknown pairing version");
        ensure!(
            value.expires_unix > now && value.expires_unix <= now.saturating_add(900),
            "expired or invalid expiry"
        );
        ensure!(
            hex::decode(&value.session)?.len() == 16,
            "invalid session id"
        );
        ensure!(
            hex::decode(&value.admission)?.len() == 16,
            "invalid admission token"
        );
        value.address()?;
        Ok(value)
    }

    /// Supply the relay URL directly: a successful DHT lookup needs no DNS follow-up.
    pub fn address(&self) -> Result<EndpointAddr> {
        let id: EndpointId = self.endpoint.parse()?;
        let relay: RelayUrl = self.relay.parse()?;
        Ok(EndpointAddr::new(id).with_relay_url(relay))
    }
}
