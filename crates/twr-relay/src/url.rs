//! Relay URLs: `relay://<host>[:<port>][#<pin>]` (docs/relay-protocol.md §3.4).

use crate::names;
use anyhow::{bail, Context, Result};
use std::fmt;
use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Name(String),
    Ip(IpAddr),
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Host::Name(n) => f.write_str(n),
            Host::Ip(IpAddr::V6(a)) => write!(f, "[{a}]"),
            Host::Ip(a) => write!(f, "{a}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUrl {
    pub host: Host,
    pub port: u16,
    /// With a pin the relay is checked by its key only; without, by the WebPKI roots.
    pub pin: Option<[u8; 32]>,
}

/// twr's own relays, as a release ships them (§3.4).
pub const TWR_RELAYS: &[&str] = &["relay://eu.relay.twr.tools", "relay://us.relay.twr.tools"];

impl RelayUrl {
    pub fn parse(text: &str) -> Result<Self> {
        let rest = text
            .strip_prefix("relay://")
            .context("a relay URL starts with relay://")?;
        let (authority, pin) = match rest.split_once('#') {
            Some((a, p)) => {
                let pin = names::parse_pin(p)
                    .context("the pin after # must be 43 base64url characters")?;
                (a, Some(pin))
            }
            None => (rest, None),
        };
        if authority.contains(['/', '?', '@', ' ']) || authority.is_empty() {
            bail!("a relay URL has a host and an optional port only");
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (addr, after) = v6.split_once(']').context("unclosed [ in the relay URL")?;
            let ip: IpAddr = addr.parse().context("bad IPv6 address")?;
            if !ip.is_ipv6() {
                bail!("brackets hold an IPv6 address");
            }
            let port = match after {
                "" => 443,
                p => p
                    .strip_prefix(':')
                    .context("expected :port")?
                    .parse()
                    .context("bad port")?,
            };
            (Host::Ip(ip), port)
        } else {
            let (h, port) = match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse::<u16>().context("bad port")?),
                None => (authority, 443),
            };
            if let Ok(ip) = h.parse::<IpAddr>() {
                if ip.is_ipv6() {
                    bail!("an IPv6 address goes in brackets");
                }
                (Host::Ip(ip), port)
            } else {
                let name = h.trim_end_matches('.').to_ascii_lowercase();
                let ok = !name.is_empty()
                    && name.len() <= 253
                    && name.split('.').all(|l| {
                        !l.is_empty()
                            && l.len() <= 63
                            && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                            && !l.starts_with('-')
                            && !l.ends_with('-')
                    });
                if !ok {
                    bail!("bad host name {h:?}");
                }
                (Host::Name(name), port)
            }
        };
        if port == 0 {
            bail!("port 0");
        }
        Ok(Self { host, port, pin })
    }

    /// The host as a TLS server name, or `None` for an IP address (no SNI).
    pub fn server_name(&self) -> Option<&str> {
        match &self.host {
            Host::Name(n) => Some(n),
            Host::Ip(_) => None,
        }
    }

    /// `host:port` for connecting.
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl fmt::Display for RelayUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "relay://{}", self.host)?;
        if self.port != 443 {
            write!(f, ":{}", self.port)?;
        }
        if let Some(pin) = &self.pin {
            write!(f, "#{}", names::pin_text(pin))?;
        }
        Ok(())
    }
}

impl std::str::FromStr for RelayUrl {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_forms() {
        let pin = names::pin_text(&[0xab; 32]);
        let u = RelayUrl::parse("relay://eu.relay.twr.tools").unwrap();
        assert_eq!(
            (u.host.clone(), u.port, u.pin),
            (Host::Name("eu.relay.twr.tools".into()), 443, None)
        );
        assert_eq!(u.to_string(), "relay://eu.relay.twr.tools");
        let u = RelayUrl::parse(&format!("relay://203.0.113.9:8443#{pin}")).unwrap();
        assert_eq!(u.server_name(), None);
        assert_eq!(u.port, 8443);
        assert_eq!(u.to_string(), format!("relay://203.0.113.9:8443#{pin}"));
        let u = RelayUrl::parse(&format!("relay://[2001:db8::1]#{pin}")).unwrap();
        assert_eq!(u.authority(), "[2001:db8::1]:443");
        assert_eq!(RelayUrl::parse(&u.to_string()).unwrap(), u);
        let u = RelayUrl::parse("relay://Relay.Example.COM.:444").unwrap();
        assert_eq!(u.server_name(), Some("relay.example.com"));
    }

    #[test]
    fn refuses_extras() {
        for bad in [
            "https://x.example",
            "relay://",
            "relay://x.example/path",
            "relay://x.example?q",
            "relay://u@x.example",
            "relay://x.example#short",
            "relay://2001:db8::1",
            "relay://[203.0.113.1]",
            "relay://x.example:0",
            "relay://x.example:99999",
            "relay://-x.example",
            "relay://x..example",
        ] {
            assert!(RelayUrl::parse(bad).is_err(), "{bad}");
        }
    }
}
