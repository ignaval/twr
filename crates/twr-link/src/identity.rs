//! Full-key workspace certificates, co-signed by machine L and workspace K.
use anyhow::{ensure, Result};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use twr_core::sealed::HubKey;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub version: u32,
    pub machine: String,
    pub workspace_key: String,
    pub endpoint: String,
    pub workspace: String,
    pub generation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Certificate {
    pub binding: Binding,
    pub machine_signature: String,
    pub workspace_signature: String,
}
impl Binding {
    fn bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = b"twr workspace link certificate v1\0".to_vec();
        bytes.extend(serde_json::to_vec(self)?);
        Ok(bytes)
    }
}
impl Certificate {
    pub fn issue(
        machine: &HubKey,
        workspace: &HubKey,
        endpoint: iroh::EndpointId,
        workspace_id: String,
        generation: String,
    ) -> Result<Self> {
        let binding = Binding {
            version: 1,
            machine: machine.public_hex(),
            workspace_key: workspace.public_hex(),
            endpoint: endpoint.to_string(),
            workspace: workspace_id,
            generation,
        };
        let bytes = binding.bytes()?;
        Ok(Self {
            binding,
            machine_signature: hex::encode(machine.sign(&bytes)),
            workspace_signature: hex::encode(workspace.sign(&bytes)),
        })
    }
    pub fn verify(&self, peer: iroh::EndpointId) -> Result<[u8; 32]> {
        let b = &self.binding;
        ensure!(
            b.version == 1 && b.endpoint.parse::<iroh::EndpointId>()? == peer,
            "workspace endpoint binding mismatch"
        );
        ensure!(
            valid_id(&b.workspace) && valid_id(&b.generation),
            "invalid workspace identity"
        );
        let machine: [u8; 32] = hex::decode(&b.machine)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid machine key"))?;
        let workspace: [u8; 32] = hex::decode(&b.workspace_key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid workspace key"))?;
        ensure!(
            b.machine == hex::encode(machine) && b.workspace_key == hex::encode(workspace),
            "identity keys must use canonical full-key encoding"
        );
        let bytes = b.bytes()?;
        for (key, sig) in [
            (machine, &self.machine_signature),
            (workspace, &self.workspace_signature),
        ] {
            UnparsedPublicKey::new(&ED25519, key)
                .verify(&bytes, &hex::decode(sig)?)
                .map_err(|_| anyhow::anyhow!("invalid workspace certificate signature"))?;
        }
        Ok(workspace)
    }
    pub fn scope(&self) -> String {
        // Full L key + full workspace identity, never the abbreviated hub_id for authority.
        format!(
            "{}/{}-{}",
            self.binding.machine, self.binding.workspace, self.binding.generation
        )
    }
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_identity_field_and_both_signatures_are_bound() -> Result<()> {
        let m = HubKey::generate()?;
        let w = HubKey::generate()?;
        let e = iroh::SecretKey::generate().public();
        let cert = Certificate::issue(&m, &w, e, "01".repeat(16), "02".repeat(16))?;
        assert_eq!(cert.verify(e)?, w.public());
        assert!(cert.verify(iroh::SecretKey::generate().public()).is_err());
        for field in 0..6 {
            let mut bad = cert.clone();
            match field {
                0 => bad.binding.machine = w.public_hex(),
                1 => bad.binding.workspace_key = m.public_hex(),
                2 => bad.binding.workspace = "03".repeat(16),
                3 => bad.binding.generation = "04".repeat(16),
                4 => bad.binding.version = 2,
                _ => bad.machine_signature = bad.workspace_signature.clone(),
            }
            assert!(bad.verify(e).is_err());
        }
        Ok(())
    }
}
