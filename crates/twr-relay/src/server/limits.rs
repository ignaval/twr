//! Rate limits, per-address counts and deny lists (docs/relay-protocol.md §11).

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A source address as the limits count it: IPv4, or the IPv6 /64 (§11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IpKey {
    V4(u32),
    V6(u64),
}

impl IpKey {
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(a) => IpKey::V4(u32::from(a)),
            IpAddr::V6(a) => match a.to_ipv4_mapped() {
                Some(v4) => IpKey::V4(u32::from(v4)),
                None => IpKey::V6((u128::from(a) >> 64) as u64),
            },
        }
    }
}

/// A token bucket. `take` says how long to wait before `n` more units are allowed.
#[derive(Debug)]
pub struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    pub fn new(rate_per_sec: f64, burst: f64) -> Self {
        Self {
            rate: rate_per_sec,
            burst,
            tokens: burst,
            last: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        self.last = now;
    }

    /// Takes `n` units now if available.
    pub fn try_take(&mut self, n: f64) -> bool {
        self.refill(Instant::now());
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Takes `n` units, going into debt if needed, and returns how long the caller should
    /// wait for the debt to clear.
    pub fn take(&mut self, n: f64) -> Duration {
        self.refill(Instant::now());
        self.tokens -= n;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate)
        }
    }

    fn full(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= self.burst
    }
}

/// `count` events per `period` for each address, as a token bucket per address.
#[derive(Debug)]
pub struct PerIp {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<IpKey, Bucket>>,
}

impl PerIp {
    pub fn new(count: u32, period: Duration) -> Self {
        let count = f64::from(count.max(1));
        Self {
            rate: count / period.as_secs_f64(),
            burst: count,
            buckets: Mutex::default(),
        }
    }

    pub fn allow(&self, ip: IpAddr) -> bool {
        let mut b = self.buckets.lock().unwrap();
        b.entry(IpKey::of(ip))
            .or_insert_with(|| Bucket::new(self.rate, self.burst))
            .try_take(1.0)
    }

    /// Drops buckets that have filled up again, which hold no state worth keeping.
    pub fn sweep(&self) {
        let now = Instant::now();
        self.buckets.lock().unwrap().retain(|_, b| !b.full(now));
    }
}

/// A count of something open per address, with a cap.
#[derive(Debug, Default)]
pub struct Concurrent {
    counts: Mutex<HashMap<IpKey, u32>>,
}

pub struct ConcurrentGuard {
    owner: Arc<Concurrent>,
    key: IpKey,
}

impl Concurrent {
    pub fn enter(self: &Arc<Self>, ip: IpAddr, cap: u32) -> Option<ConcurrentGuard> {
        let key = IpKey::of(ip);
        let mut c = self.counts.lock().unwrap();
        let n = c.entry(key).or_insert(0);
        if *n >= cap {
            return None;
        }
        *n += 1;
        Some(ConcurrentGuard {
            owner: self.clone(),
            key,
        })
    }
}

impl Drop for ConcurrentGuard {
    fn drop(&mut self) {
        let mut c = self.owner.counts.lock().unwrap();
        if let Some(n) = c.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                c.remove(&self.key);
            }
        }
    }
}

/// An address prefix for the deny list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    addr: u128,
    len: u8,
    v4: bool,
}

impl Prefix {
    pub fn parse(text: &str) -> Result<Self> {
        let (a, len) = match text.split_once('/') {
            Some((a, l)) => (a, Some(l.parse::<u8>().context("bad prefix length")?)),
            None => (text, None),
        };
        let ip: IpAddr = a.parse().context("bad address")?;
        let (addr, max, v4) = match ip {
            IpAddr::V4(v) => (u128::from(u32::from(v)) << 96, 32u8, true),
            IpAddr::V6(v) => (u128::from(v), 128u8, false),
        };
        let len = len.unwrap_or(max);
        if len > max {
            bail!("prefix length over {max}");
        }
        Ok(Self {
            addr: mask(addr, len),
            len,
            v4,
        })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let (addr, v4) = match ip {
            IpAddr::V4(v) => (u128::from(u32::from(v)) << 96, true),
            IpAddr::V6(v) => match v.to_ipv4_mapped() {
                Some(m) => (u128::from(u32::from(m)) << 96, true),
                None => (u128::from(v), false),
            },
        };
        v4 == self.v4 && mask(addr, self.len) == self.addr
    }
}

fn mask(addr: u128, len: u8) -> u128 {
    if len == 0 {
        0
    } else {
        addr & (u128::MAX << (128 - u32::from(len)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_keys_group_v6_by_64() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(IpKey::of(a), IpKey::of(b));
        assert_ne!(IpKey::of(a), IpKey::of(c));
        let m: IpAddr = "::ffff:198.51.100.7".parse().unwrap();
        assert_eq!(IpKey::of(m), IpKey::of("198.51.100.7".parse().unwrap()));
    }

    #[test]
    fn per_ip_and_concurrent() {
        let p = PerIp::new(3, Duration::from_secs(3600));
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        assert!(p.allow(ip) && p.allow(ip) && p.allow(ip));
        assert!(!p.allow(ip));
        assert!(p.allow("192.0.2.2".parse().unwrap()));
        let c = Arc::new(Concurrent::default());
        let g1 = c.enter(ip, 2).unwrap();
        let _g2 = c.enter(ip, 2).unwrap();
        assert!(c.enter(ip, 2).is_none());
        drop(g1);
        assert!(c.enter(ip, 2).is_some());
    }

    #[test]
    fn prefixes() {
        let p = Prefix::parse("198.51.100.0/24").unwrap();
        assert!(p.contains("198.51.100.200".parse().unwrap()));
        assert!(p.contains("::ffff:198.51.100.1".parse().unwrap()));
        assert!(!p.contains("198.51.101.1".parse().unwrap()));
        let q = Prefix::parse("2001:db8::/32").unwrap();
        assert!(q.contains("2001:db8:ffff::1".parse().unwrap()));
        assert!(!q.contains("2001:db9::1".parse().unwrap()));
        assert!(Prefix::parse("1.2.3.4/33").is_err());
        assert!(Prefix::parse("1.2.3.4")
            .unwrap()
            .contains("1.2.3.4".parse().unwrap()));
        assert!(Prefix::parse("0.0.0.0/0")
            .unwrap()
            .contains("8.8.8.8".parse().unwrap()));
        assert!(!Prefix::parse("0.0.0.0/0")
            .unwrap()
            .contains("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn bucket_debt() {
        let mut b = Bucket::new(1000.0, 1000.0);
        assert_eq!(b.take(1000.0), Duration::ZERO);
        let w = b.take(500.0);
        assert!(
            w > Duration::from_millis(400) && w <= Duration::from_millis(500),
            "{w:?}"
        );
    }
}
