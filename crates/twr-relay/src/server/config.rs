//! `relay.toml`, the relay's one configuration file (docs/relay.md).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub relay: RelaySection,
    #[serde(default)]
    pub tls: TlsSection,
    #[serde(default)]
    pub iroh: IrohSection,
    #[serde(default)]
    pub limits: LimitsSection,
    #[serde(default)]
    pub deny: DenySection,
    #[serde(default, rename = "port_hub")]
    pub port_hubs: Vec<PortHub>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelaySection {
    /// The relay's own DNS name, for example `eu.relay.twr.tools`. Without one the relay runs
    /// without DNS: phones use `relay.invalid` or port mode (docs/relay-protocol.md §7).
    pub hostname: Option<String>,
    /// The address phones and hubs are told to connect to, when it differs from `hostname`
    /// (always needed without a hostname: the public IP).
    pub public_host: Option<String>,
    /// The public port, if a port forward puts the relay behind another one.
    pub public_port: Option<u16>,
    /// A short region name shown to hubs, for example `eu`.
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default = "default_listen")]
    pub listen: Vec<SocketAddr>,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// Counters for monitoring, as Prometheus text at `/metrics`. Loopback only unless
    /// `metrics_public` is set.
    #[serde(default = "default_metrics")]
    pub metrics_listen: Option<SocketAddr>,
    #[serde(default)]
    pub metrics_public: bool,
    /// Phone suffixes; the default is `hostname`, or `relay.invalid` without one.
    #[serde(default)]
    pub phone_suffixes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    /// Let's Encrypt with tls-alpn-01; needs `hostname`.
    #[default]
    Acme,
    /// A key and certificate the relay makes itself; hubs pin it.
    SelfSigned,
    /// PEM files from elsewhere.
    Files,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSection {
    #[serde(default)]
    pub mode: TlsMode,
    /// `mailto:` contact for the ACME account, for example `mailto:relay-ops@twr.tools`.
    pub acme_contact: Option<String>,
    #[serde(default = "default_acme_directory")]
    pub acme_directory: String,
    /// Extra PEM roots to trust for the ACME directory, for a local test CA such as pebble.
    pub acme_ca_roots: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrohSection {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Default `iroh.<hostname>`.
    pub hostname: Option<String>,
    /// iroh's QUIC address discovery (UDP). `None` turns it off.
    #[serde(default = "default_quic")]
    pub quic_listen: Option<SocketAddr>,
    /// Without a hostname, iroh can't share the main port (§10): it gets this one.
    pub listen: Option<SocketAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSection {
    #[serde(default = "d32")]
    pub phones_per_hub: u32,
    #[serde(default = "d8")]
    pub phone_connections_per_hub: u32,
    #[serde(default = "d_monthly")]
    pub monthly_bytes_per_hub: u64,
    #[serde(default = "d_rate")]
    pub bytes_per_second: u64,
    #[serde(default = "d_burst")]
    pub burst_bytes: u64,
    #[serde(default = "d_idle")]
    pub idle_seconds: u64,
    #[serde(default = "d5")]
    pub hello_seconds: u64,
    #[serde(default = "d10s")]
    pub hub_hello_seconds: u64,
    #[serde(default = "d10")]
    pub new_hubs_per_ip_per_hour: u32,
    #[serde(default = "d60")]
    pub phone_attempts_per_ip_per_minute: u32,
    #[serde(default = "d16")]
    pub pending_per_ip: u32,
    #[serde(default = "d60")]
    pub record_puts_per_ip_per_minute: u32,
    #[serde(default = "d10")]
    pub pairing_admissions_per_ip_per_hour: u32,
    #[serde(default = "d60")]
    pub pair_admissions_per_ip_per_hour: u32,
    #[serde(default = "d_conns")]
    pub max_connections: u32,
    #[serde(default = "d_hubs")]
    pub max_hubs: u32,
    #[serde(default = "d_records")]
    pub max_records: u32,
    #[serde(default = "d_admitted")]
    pub max_admitted: u32,
    #[serde(default = "d_record_ttl")]
    pub record_ttl_seconds: u64,
    /// Addresses or prefixes the per-address limits don't apply to, such as the monitoring
    /// hosts that probe the relay every minute. The deny list still does.
    #[serde(default)]
    pub exempt_addresses: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DenySection {
    #[serde(default)]
    pub hubs: Vec<String>,
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// Addresses or CIDR prefixes, for example `198.51.100.0/24`.
    #[serde(default)]
    pub addresses: Vec<String>,
}

/// A port of its own for one hub (docs/relay-protocol.md §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortHub {
    pub hub: String,
    pub listen: SocketAddr,
    /// The public port to announce, if different from `listen`'s.
    pub public_port: Option<u16>,
    #[serde(default = "yes")]
    pub allow_unauthenticated: bool,
}

fn yes() -> bool {
    true
}
fn default_region() -> String {
    "default".into()
}
fn default_listen() -> Vec<SocketAddr> {
    vec!["[::]:443".parse().unwrap()]
}
fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/twr-relay")
}
fn default_metrics() -> Option<SocketAddr> {
    Some("127.0.0.1:9469".parse().unwrap())
}
fn default_quic() -> Option<SocketAddr> {
    Some("[::]:7842".parse().unwrap())
}
fn default_acme_directory() -> String {
    "https://acme-v02.api.letsencrypt.org/directory".into()
}
fn d5() -> u64 {
    5
}
fn d8() -> u32 {
    8
}
fn d10() -> u32 {
    10
}
fn d10s() -> u64 {
    10
}
fn d16() -> u32 {
    16
}
fn d32() -> u32 {
    32
}
fn d60() -> u32 {
    60
}
fn d_monthly() -> u64 {
    20_000_000_000
}
fn d_rate() -> u64 {
    10_000_000
}
fn d_burst() -> u64 {
    32_000_000
}
fn d_idle() -> u64 {
    600
}
fn d_conns() -> u32 {
    20_000
}
fn d_hubs() -> u32 {
    50_000
}
fn d_records() -> u32 {
    100_000
}
fn d_admitted() -> u32 {
    200_000
}
fn d_record_ttl() -> u64 {
    7200
}

impl Default for RelaySection {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}
impl Default for TlsSection {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}
impl Default for IrohSection {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}
impl Default for LimitsSection {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.relay.listen.is_empty() {
            bail!("relay.listen is empty");
        }
        if let Some(h) = &self.relay.hostname {
            crate::RelayUrl::parse(&format!("relay://{h}"))
                .with_context(|| format!("relay.hostname {h:?}"))?;
            if h.parse::<IpAddr>().is_ok() {
                bail!("relay.hostname is a DNS name; put an IP address in relay.public_host");
            }
        } else if self.tls.mode == TlsMode::Acme {
            bail!("tls.mode = \"acme\" needs relay.hostname; use \"self-signed\" for a relay without DNS");
        }
        if self.relay.hostname.is_none() && self.relay.public_host.is_none() {
            bail!("a relay without relay.hostname needs relay.public_host, its public IP address");
        }
        if self.tls.mode == TlsMode::Files && (self.tls.cert.is_none() || self.tls.key.is_none()) {
            bail!("tls.mode = \"files\" needs tls.cert and tls.key");
        }
        if let Some(c) = &self.tls.acme_contact {
            if !c.starts_with("mailto:") {
                bail!("tls.acme_contact must start with mailto:");
            }
        }
        if let Some(m) = self.relay.metrics_listen {
            if !m.ip().is_loopback() && !self.relay.metrics_public {
                bail!("relay.metrics_listen is not a loopback address; set metrics_public = true to allow that");
            }
        }
        if self.iroh.enabled && self.relay.hostname.is_none() && self.iroh.listen.is_none() {
            bail!("iroh without relay.hostname needs iroh.listen, a port of its own (or iroh.enabled = false)");
        }
        for p in &self.port_hubs {
            if !crate::names::is_hub_name(&p.hub) {
                bail!("port_hub.hub {:?} is not a hub name", p.hub);
            }
        }
        for a in self
            .deny
            .addresses
            .iter()
            .chain(&self.limits.exempt_addresses)
        {
            super::limits::Prefix::parse(a).with_context(|| format!("deny.addresses {a:?}"))?;
        }
        if self.limits.bytes_per_second == 0 || self.limits.burst_bytes < 65536 {
            bail!("limits.bytes_per_second must be positive and limits.burst_bytes at least 65536");
        }
        Ok(())
    }

    pub fn phone_suffixes(&self) -> Vec<String> {
        if !self.relay.phone_suffixes.is_empty() {
            return self
                .relay
                .phone_suffixes
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect();
        }
        vec![self
            .relay
            .hostname
            .clone()
            .unwrap_or_else(|| "relay.invalid".into())]
    }

    pub fn iroh_hostname(&self) -> Option<String> {
        if !self.iroh.enabled {
            return None;
        }
        self.iroh
            .hostname
            .clone()
            .or_else(|| self.relay.hostname.as_ref().map(|h| format!("iroh.{h}")))
    }

    /// The host phones and hubs are told to connect to.
    pub fn public_host(&self) -> String {
        self.relay
            .public_host
            .clone()
            .or_else(|| self.relay.hostname.clone())
            .unwrap_or_default()
    }

    pub fn public_port(&self) -> u16 {
        self.relay
            .public_port
            .unwrap_or_else(|| self.relay.listen[0].port())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_validation() {
        let c: Config = toml::from_str("[relay]\nhostname = \"eu.relay.example\"\n").unwrap();
        c.validate().unwrap();
        assert_eq!(c.phone_suffixes(), vec!["eu.relay.example"]);
        assert_eq!(c.iroh_hostname().as_deref(), Some("iroh.eu.relay.example"));
        assert_eq!(c.limits.phones_per_hub, 32);
        assert_eq!(c.limits.bytes_per_second, 10_000_000);

        let bare = Config::default();
        assert!(bare.validate().is_err(), "acme without hostname");

        let c: Config = toml::from_str(
            "[relay]\npublic_host = \"203.0.113.9\"\n[tls]\nmode = \"self-signed\"\n[iroh]\nlisten = \"[::]:7443\"\n",
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.phone_suffixes(), vec!["relay.invalid"]);
        assert!(toml::from_str::<Config>("[relay]\nbogus = 1\n").is_err());
        let c: Config = toml::from_str(
            "[relay]\nhostname = \"r.example\"\nmetrics_listen = \"0.0.0.0:9469\"\n",
        )
        .unwrap();
        assert!(c.validate().is_err());
    }
}
