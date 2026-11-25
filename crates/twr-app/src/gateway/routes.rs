//! The routes a pinned gateway offers (docs/design/phone-pairing.md#invitation-version-2): the
//! configured address first, then this machine's addresses that its listeners reach, global
//! IPv4, global IPv6, tailnet and private LAN, each class within its quota, at most
//! [`MAX_ROUTES`].
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

/// The classes in the order a list takes them, each with the most its first pass takes, so
/// that one class (a host's many global IPv6 addresses, say) never pushes the others out:
/// global IPv4, global IPv6, tailnet, private LAN.
const QUOTAS: [(Class, Option<bool>, usize); 4] = [
    (Class::Global, Some(false), 2),
    (Class::Global, Some(true), 2),
    (Class::Tailnet, None, 2),
    (Class::Private, None, 3),
];

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
/// (`0.0.0.0`, `[::]`). Those go by [`QUOTAS`]: at most 2 global IPv4, 2 global IPv6, 2
/// tailnet and 3 private (IPv4 before IPv6 within a class), then whatever is left, in the
/// same order, while there is room. Duplicates dropped, at most [`MAX_ROUTES`].
pub fn order(address: Option<&str>, listeners: &[SocketAddr], interfaces: &[IpAddr]) -> Vec<Route> {
    let port = listeners.first().map_or(47321, SocketAddr::port);
    let configured = address.and_then(|a| configured(a, port).ok());
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
    let mut routes: Vec<Route> = configured.into_iter().collect();
    // Each candidate once, the configured address's twin left out, before any quota counts it.
    let mut candidates: Vec<(Class, bool, Route)> = Vec::new();
    for (class, v6, ip, port) in found {
        let route = Route::Tls {
            host: ip.to_string(),
            port,
        };
        if !routes.contains(&route) && !candidates.iter().any(|(.., r)| *r == route) {
            candidates.push((class, v6, route));
        }
    }
    let mut taken = vec![false; candidates.len()];
    // First each class up to its quota, then the rest in the same order.
    for quota in [true, false] {
        for (class, family, most) in QUOTAS {
            let mut count = 0;
            for (i, (c, v6, route)) in candidates.iter().enumerate() {
                if routes.len() == MAX_ROUTES || (quota && count == most) {
                    break;
                }
                if taken[i] || *c != class || family.is_some_and(|f| f != *v6) {
                    continue;
                }
                taken[i] = true;
                count += 1;
                routes.push(route.clone());
            }
        }
    }
    routes
}

/// Whether interface `name` is a virtual bridge of containers or virtual machines on this
/// machine (Docker, Podman, libvirt, LXC, CNI, flannel, and their `veth` ends), whose
/// addresses no phone reaches.
pub fn virtual_interface(name: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "docker", "br-", "veth", "virbr", "cni", "podman", "lxcbr", "flannel",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The IPv6 addresses Linux's `/proc/net/if_inet6` (its text `if_inet6`) marks as not to be
/// offered: temporary (privacy) ones, which change daily, deprecated ones, and those whose
/// duplicate address detection is still running or failed. Lines are `<32 hex digits>
/// <ifindex> <prefix length> <scope> <flags> <name>`, numbers in hex.
pub fn unusable_ipv6(if_inet6: &str) -> std::collections::HashSet<Ipv6Addr> {
    // IFA_F_TEMPORARY, IFA_F_DADFAILED, IFA_F_DEPRECATED, IFA_F_TENTATIVE.
    const UNUSABLE: u32 = 0x01 | 0x08 | 0x20 | 0x40;
    if_inet6
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (address, flags) = (fields.first()?, fields.get(4)?);
            let flags = u32::from_str_radix(flags, 16).ok()?;
            let address = u128::from_str_radix(address, 16)
                .ok()
                .filter(|_| address.len() == 32)?;
            (flags & UNUSABLE != 0).then_some(Ipv6Addr::from(address))
        })
        .collect()
}

/// Whether the system marks IPv6 address `addr` of interface `name` (a NUL-terminated name)
/// temporary, deprecated, tentative, duplicated or detached (`SIOCGIFAFLAG_IN6`). On error,
/// `false`: the address is offered.
#[cfg(target_os = "macos")]
fn unusable_ipv6_on(socket: libc::c_int, name: &std::ffi::CStr, addr: &libc::sockaddr_in6) -> bool {
    const UNUSABLE: libc::c_int = libc::IN6_IFF_TEMPORARY
        | libc::IN6_IFF_DEPRECATED
        | libc::IN6_IFF_TENTATIVE
        | libc::IN6_IFF_DUPLICATED
        | libc::IN6_IFF_DETACHED;
    let bytes = name.to_bytes();
    if socket < 0 || bytes.len() >= libc::IFNAMSIZ {
        return false;
    }
    // SAFETY: `request` is a zeroed `in6_ifreq` (all-zero is valid for it) holding the name,
    // NUL-terminated within IFNAMSIZ, and the address; the ioctl reads and writes only it.
    unsafe {
        let mut request: libc::in6_ifreq = std::mem::zeroed();
        for (to, from) in request.ifr_name.iter_mut().zip(bytes) {
            *to = *from as libc::c_char;
        }
        request.ifr_ifru.ifru_addr = *addr;
        if libc::ioctl(socket, libc::SIOCGIFAFLAG_IN6, &mut request) != 0 {
            return false;
        }
        request.ifr_ifru.ifru_flags6 & UNUSABLE != 0
    }
}

/// The addresses of this machine's interfaces that are up, to offer phones (`getifaddrs`):
/// loopback interfaces and virtual bridges ([`virtual_interface`]) left out, and IPv6
/// addresses the system marks temporary, deprecated or not yet (or never) usable: from
/// `/proc/net/if_inet6` on Linux ([`unusable_ipv6`]), `SIOCGIFAFLAG_IN6` on macOS.
pub fn interface_addresses() -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    #[cfg(target_os = "linux")]
    let unusable = std::fs::read_to_string("/proc/net/if_inet6")
        .map(|text| unusable_ipv6(&text))
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    // SAFETY: a plain socket, closed below.
    let socket = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) };
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, which freeifaddrs releases below; every entry is read
    // only while the list lives.
    unsafe {
        if libc::getifaddrs(&mut list) == 0 {
            let mut entry = list;
            while !entry.is_null() {
                let ifa = &*entry;
                entry = ifa.ifa_next;
                let flags = ifa.ifa_flags as libc::c_int;
                if ifa.ifa_addr.is_null()
                    || ifa.ifa_name.is_null()
                    || flags & libc::IFF_UP == 0
                    || flags & libc::IFF_LOOPBACK != 0
                {
                    continue;
                }
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name);
                if virtual_interface(&name.to_string_lossy()) {
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
                        let ip = Ipv6Addr::from(addr.sin6_addr.s6_addr);
                        #[cfg(target_os = "linux")]
                        if unusable.contains(&ip) {
                            continue;
                        }
                        #[cfg(target_os = "macos")]
                        if unusable_ipv6_on(socket, name, addr) {
                            continue;
                        }
                        addresses.push(IpAddr::V6(ip));
                    }
                    _ => {}
                }
            }
            libc::freeifaddrs(list);
        }
    }
    #[cfg(target_os = "macos")]
    if socket >= 0 {
        // SAFETY: the socket opened above, closed once.
        unsafe { libc::close(socket) };
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

    /// No class pushes the others out: each first gets its quota (2 global IPv4, 2 global
    /// IPv6, 2 tailnet, 3 private), then the slots left go to the rest, in the same order.
    #[test]
    fn every_class_gets_its_quota_before_any_fills_the_rest() {
        let crowded = ips(&[
            "2001:db8::1",
            "2001:db8::2",
            "2001:db8::3",
            "2001:db8::4",
            "2001:db8::5",
            "2001:db8::6",
            "203.0.113.1",
            "203.0.113.2",
            "203.0.113.3",
            "fd7a:115c:a1e0::9",
            "100.64.0.1",
            "100.64.0.2",
            "fd00::1",
            "192.168.1.2",
            "10.0.0.3",
            "172.16.0.4",
        ]);
        assert_eq!(
            texts(&order(None, &all(), &crowded)),
            [
                "tls:203.0.113.1:47321",
                "tls:203.0.113.2:47321",
                "tls:[2001:db8::1]:47321",
                "tls:[2001:db8::2]:47321",
                "tls:100.64.0.1:47321",
                "tls:100.64.0.2:47321",
                "tls:192.168.1.2:47321",
                "tls:10.0.0.3:47321",
            ]
        );
        // The configured address takes a slot first.
        let routes = texts(&order(Some("box.example.com"), &all(), &crowded));
        assert_eq!(routes.len(), 8);
        assert_eq!(routes[0], "tls:box.example.com:47321");
        assert_eq!(routes[7], "tls:192.168.1.2:47321");
        // Classes with fewer than their quota leave room for others' extras, in class order.
        let few = ips(&[
            "2001:db8::1",
            "2001:db8::2",
            "2001:db8::3",
            "2001:db8::4",
            "2001:db8::5",
            "192.168.1.2",
            "203.0.113.1",
        ]);
        assert_eq!(
            texts(&order(None, &all(), &few)),
            [
                "tls:203.0.113.1:47321",
                "tls:[2001:db8::1]:47321",
                "tls:[2001:db8::2]:47321",
                "tls:192.168.1.2:47321",
                "tls:[2001:db8::3]:47321",
                "tls:[2001:db8::4]:47321",
                "tls:[2001:db8::5]:47321",
            ]
        );
        // A duplicate takes no slot of its class.
        let twice = ips(&["203.0.113.1", "203.0.113.1", "203.0.113.2", "203.0.113.3"]);
        let v4 = ["0.0.0.0:47321".parse().unwrap()];
        assert_eq!(
            texts(&order(Some("203.0.113.1"), &v4, &twice)),
            [
                "tls:203.0.113.1:47321",
                "tls:203.0.113.2:47321",
                "tls:203.0.113.3:47321",
            ]
        );
    }

    #[test]
    fn container_and_vm_bridges_are_not_offered() {
        for name in [
            "docker0",
            "br-d875f32208ce",
            "veth4d95606",
            "virbr0",
            "cni0",
            "podman1",
            "lxcbr0",
            "flannel.1",
        ] {
            assert!(virtual_interface(name), "{name}");
        }
        for name in [
            "eth0",
            "en0",
            "wlan0",
            "wlp2s0",
            "tailscale0",
            "utun3",
            "bridge0",
        ] {
            assert!(!virtual_interface(name), "{name}");
        }
    }

    /// Temporary, deprecated, tentative and DAD-failed IPv6 addresses are left out; stable and
    /// link-local ones, and lines that don't parse, aren't marked.
    #[test]
    fn if_inet6_flags_mark_the_addresses_not_to_offer() {
        let text = "\
fe80000000000000646fb0fffeab5c76 119 40 20 80 veth4d95606
2a024780001454cf0000000000000001 02 30 00 80     eth0
20010db8000000000000000000000001 03 40 00 00     wlan0
20010db8000000001c2a3b4c5d6e7f01 03 40 00 01     wlan0
20010db8000000002c2a3b4c5d6e7f02 03 40 00 21     wlan0
20010db8000000003c2a3b4c5d6e7f03 03 40 00 a0     wlan0
20010db8000000004c2a3b4c5d6e7f04 03 40 00 c0     wlan0
20010db8000000005c2a3b4c5d6e7f05 03 40 00 88     wlan0
00000000000000000000000000000001 01 80 10 80       lo
not a line
20010db8 03 40 00 01 short
";
        let unusable = unusable_ipv6(text);
        let expected: std::collections::HashSet<Ipv6Addr> = [
            "2001:db8::1c2a:3b4c:5d6e:7f01",
            "2001:db8:0:0:2c2a:3b4c:5d6e:7f02",
            "2001:db8:0:0:3c2a:3b4c:5d6e:7f03",
            "2001:db8:0:0:4c2a:3b4c:5d6e:7f04",
            "2001:db8:0:0:5c2a:3b4c:5d6e:7f05",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        assert_eq!(unusable, expected);
        assert!(unusable_ipv6("").is_empty());
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
