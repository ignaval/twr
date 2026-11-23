//! The pinned-mode gateway's saved settings, `gateway-settings.json` in its directory (0600):
//! the address invitations offer first, where it listens, and whether a local hub starts it.
//! `twr phone` writes them all, and alone turns autostart on; a pinned gateway given
//! `--address` saves that, and `--stop-gateway` turns autostart off. The desktop app keeps its
//! certificate-mode gateway's settings in its own settings file, and this one plays no part
//! there.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::Path};

pub const FILE: &str = "gateway-settings.json";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// What invitations offer first: a DNS name or a forwarded `host:port` (`--address`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Where the gateway listens when no `--listen` is given, as the hub's autostart starts
    /// it; empty for `0.0.0.0:47321` and `[::]:47321`. `twr phone` saves where the gateway it
    /// pairs through listens.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listen: Vec<SocketAddr>,
    /// Whether a local hub that starts runs the gateway, when phones are paired with it
    /// (`twr phone` alone turns it on; `twr phone off` and `--stop-gateway` off).
    #[serde(default)]
    pub autostart: bool,
}

impl Settings {
    /// The settings in `dir`; `None` when there are none (no pinned gateway ran there).
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        match twr_link::state::read_private(&dir.join(FILE), 64 * 1024) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", dir.join(FILE).display()))
                .map(Some),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Saves them in `dir`, atomically, private.
    pub fn save(&self, dir: &Path) -> Result<()> {
        crate::remote::gateway_lifecycle::private_dir(dir)?;
        twr_link::state::atomic_write(&dir.join(FILE), &serde_json::to_vec_pretty(self)?)
    }

    /// Sets the address (`--address`): `host`, `host:port`, `[IPv6]` or `[IPv6]:port`, or
    /// nothing (empty) to offer none.
    pub fn set_address(&mut self, address: &str) -> Result<()> {
        let address = address.trim();
        if address.is_empty() {
            self.address = None;
            return Ok(());
        }
        super::routes::configured(address, 47321).with_context(|| {
            format!("`{address}` is not an address: give host, host:port, [IPv6] or [IPv6]:port")
        })?;
        self.address = Some(address.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn settings_save_private_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), None);
        let mut settings = Settings {
            autostart: true,
            ..Default::default()
        };
        settings.set_address("Box.Example.com").unwrap();
        settings.save(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(Settings::load(dir.path()).unwrap(), Some(settings.clone()));
        for good in [
            "box:443",
            "192.0.2.4",
            "[2001:db8::1]:8443",
            "2001:db8::1",
            "",
        ] {
            settings.set_address(good).unwrap();
        }
        assert_eq!(settings.address, None);
        for bad in ["box:0", "-x-:1", "box:port", "[::1:2", "a b"] {
            assert!(settings.set_address(bad).is_err(), "{bad}");
        }
    }
}
