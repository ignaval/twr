//! The routes a pinned gateway offers (docs/design/phone-pairing.md#invitation-version-2): the
//! configured address first, then this machine's addresses that its listeners reach, public
//! before tailnet before private LAN, at most [`MAX_ROUTES`].
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use twr_core::phone_pairing::{Route, MAX_ROUTES};

/// The configured address `address` (`host`, `host:port`, `[IPv6]`, `[IPv6]:port`, or a bare
/// IPv6 literal) as a route, on `port` when it names none.
pub fn configured(address: &str, port: u16) -> anyhow::Result<Route> {
    let address = address.trim();
    if let Ok(ip) = address.parse::<Ipv6Addr>() {
        return Ok(Route::Tls {
            host: ip.to_string(),
            port,
        });
    }
    let with_port = if address.ends_with(']') || !address.contains(':') {
        format!("{address}:{port}")
    } else {
        address.to_owned()
    };
    match Route::parse(&format!("tls:{with_port}"))? {
        route @ Route::Tls { .. } => Ok(route),
        Route::Other(_) => anyhow::bail!("not an address"),
    }
}

/// Which addresses come first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    Global,
    Tailnet,
    Private,
}

/// Where `ip` belongs in the list, or `None` for one a phone can't use: loopback, link-local,
/// unspecified, multicast, an IPv4-mapped one.
fn class(ip: IpAddr) -> Option<Class> {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            if ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                || a == 0
            {
                None
            } else if a == 100 && (64..128).contains(&b) {
                Some(Class::Tailnet)
            } else if ip.is_private() {
                Some(Class::Private)
            } else {
                Some(Class::Global)
            }
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            if ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (s[0] & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some()
            {
                None
            } else if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                Some(Class::Tailnet)
            } else if (s[0] & 0xfe00) == 0xfc00 {
                Some(Class::Private)
            } else {
                Some(Class::Global)
            }
        }
    }
}

/// The routes to offer: `address` (configured) first, then for each listener its own address
/// when it listens on one, or the `interfaces` addresses of its family when it listens on all
/// (`0.0.0.0`, `[::]`), ordered global, tailnet, private (IPv4 before IPv6 within each);
/// duplicates dropped, at most [`MAX_ROUTES`].
pub fn order(address: Option<&str>, listeners: &[SocketAddr], interfaces: &[IpAddr]) -> Vec<Route> {
    let port = listeners.first().map_or(47321, SocketAddr::port);
    let mut routes = Vec::new();
    if let Some(route) = address.and_then(|a| configured(a, port).ok()) {
        routes.push(route);
    }
    let mut found: Vec<(Class, bool, IpAddr, u16)> = Vec::new();
    // A listener whose port the system has yet to choose has no route.
    for listener in listeners.iter().filter(|l| l.port() != 0) {
        let ip = listener.ip();
        if !ip.is_unspecified() {
            // A listener on one address: that one, whatever it is (a loopback one too).
            found.push((
                class(ip).unwrap_or(Class::Private),
                ip.is_ipv6(),
                ip,
                listener.port(),
            ));
            continue;
        }
        for &address in interfaces.iter().filter(|a| a.is_ipv6() == ip.is_ipv6()) {
            if let Some(class) = class(address) {
                found.push((class, address.is_ipv6(), address, listener.port()));
            }
        }
    }
    // Stable: interfaces keep their order within a class and family.
    found.sort_by_key(|(class, v6, ..)| (*class, *v6));
    for (_, _, ip, port) in found {
        routes.push(Route::Tls {
            host: ip.to_string(),
            port,
        });
    }
    let mut unique = Vec::new();
    for route in routes {
        if !unique.contains(&route) {
            unique.push(route);
        }
    }
    unique.truncate(MAX_ROUTES);
    unique
}

/// The addresses of this machine's interfaces that are up, loopback ones left out
/// (`getifaddrs`).
pub fn interface_addresses() -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, which freeifaddrs releases below; every entry is read
    // only while the list lives.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return addresses;
        }
        let mut entry = list;
        while !entry.is_null() {
            let ifa = &*entry;
            entry = ifa.ifa_next;
            let flags = ifa.ifa_flags as libc::c_int;
            if ifa.ifa_addr.is_null()
                || flags & libc::IFF_UP == 0
                || flags & libc::IFF_LOOPBACK != 0
            {
                continue;
            }
            match (*ifa.ifa_addr).sa_family as libc::c_int {
                libc::AF_INET => {
                    let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    addresses.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                        addr.sin_addr.s_addr,
                    ))));
                }
                libc::AF_INET6 => {
                    let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    addresses.push(IpAddr::V6(Ipv6Addr::from(addr.sin6_addr.s6_addr)));
                }
                _ => {}
            }
        }
        libc::freeifaddrs(list);
    }
    addresses
}

/// The routes the gateway of `dir` offers now, listening on `listeners`: its saved address
/// ([`super::settings::Settings`]) and this machine's addresses.
pub fn current(dir: &std::path::Path, listeners: &[SocketAddr]) -> Vec<Route> {
    let address = super::settings::Settings::load(dir)
        .ok()
        .flatten()
        .and_then(|s| s.address);
    order(address.as_deref(), listeners, &interface_addresses())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|a| a.parse().unwrap()).collect()
    }
    fn texts(routes: &[Route]) -> Vec<String> {
        routes.iter().map(Route::to_string).collect()
    }
    fn all() -> Vec<SocketAddr> {
        vec![
            "0.0.0.0:47321".parse().unwrap(),
            "[::]:47321".parse().unwrap(),
        ]
    }

    #[test]
    fn routes_come_configured_then_public_tailnet_private() {
        let interfaces = ips(&[
            "192.168.1.20",
            "127.0.0.1",
            "169.254.3.4",
            "100.101.102.103",
            "203.0.113.9",
            "10.0.0.2",
            "fe80::1",
            "::1",
            "fd7a:115c:a1e0::5",
            "fd00::7",
            "2001:db8::5",
            "172.20.0.1",
        ]);
        let routes = order(Some("box.example.com"), &all(), &interfaces);
        assert_eq!(
            texts(&routes),
            [
                "tls:box.example.com:47321",
                "tls:203.0.113.9:47321",
                "tls:[2001:db8::5]:47321",
                "tls:100.101.102.103:47321",
                "tls:[fd7a:115c:a1e0::5]:47321",
                "tls:192.168.1.20:47321",
                "tls:10.0.0.2:47321",
                "tls:172.20.0.1:47321",
            ],
            "the ninth, fd00::7, is cut off"
        );
        // A forwarded port is the address's own; IPv4 only when only it listens.
        let v4 = ["0.0.0.0:47321".parse().unwrap()];
        let routes = order(Some("example.net:443"), &v4, &interfaces);
        assert_eq!(texts(&routes)[0], "tls:example.net:443");
        assert!(texts(&routes).iter().all(|r| !r.contains('[')));
        // Nothing to offer but a listener's own address.
        let one = ["127.0.0.1:5000".parse().unwrap()];
        assert_eq!(
            texts(&order(None, &one, &interfaces)),
            ["tls:127.0.0.1:5000"]
        );
        assert!(order(None, &all(), &ips(&["127.0.0.1", "fe80::2"])).is_empty());
        let unbound = ["0.0.0.0:0".parse().unwrap()];
        assert!(order(None, &unbound, &interfaces).is_empty());
        // The configured address once, even when an interface has it too.
        let routes = order(Some("203.0.113.9"), &v4, &ips(&["203.0.113.9"]));
        assert_eq!(texts(&routes), ["tls:203.0.113.9:47321"]);
    }

    #[test]
    fn configured_addresses_take_the_listener_port_unless_they_name_one() {
        for (input, route) in [
            ("box", "tls:box:9"),
            ("box:443", "tls:box:443"),
            ("192.0.2.1", "tls:192.0.2.1:9"),
            ("[2001:db8::1]", "tls:[2001:db8::1]:9"),
            ("[2001:db8::1]:443", "tls:[2001:db8::1]:443"),
            ("2001:db8::1", "tls:[2001:db8::1]:9"),
        ] {
            assert_eq!(configured(input, 9).unwrap().to_string(), route, "{input}");
        }
        assert!(configured("box:0", 9).is_err());
    }

    #[test]
    fn this_machine_has_no_loopback_route() {
        let found = interface_addresses();
        assert!(found.iter().all(|a| !a.is_loopback()), "{found:?}");
    }
}
