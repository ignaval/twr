//! Counters, the only thing the relay records about traffic (docs/relay-protocol.md §14).
//! Exposed as Prometheus text on a loopback port.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};

/// Why a connection or request was refused. Fixed, so the counters can't grow unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotTls,
    TooLarge,
    SlowHello,
    UnknownName,
    BadLabel,
    HubOffline,
    BadTag,
    HubBusy,
    Quota,
    PhoneRate,
    Pending,
    Capacity,
    Denied,
    HubAuth,
    HubRate,
    TlsHandshake,
    RecordRejected,
    RecordRate,
    AdmitRejected,
    AdmitRate,
    IrohDenied,
    PortNoCredential,
}

impl Refusal {
    pub const ALL: [Refusal; 22] = [
        Refusal::NotTls,
        Refusal::TooLarge,
        Refusal::SlowHello,
        Refusal::UnknownName,
        Refusal::BadLabel,
        Refusal::HubOffline,
        Refusal::BadTag,
        Refusal::HubBusy,
        Refusal::Quota,
        Refusal::PhoneRate,
        Refusal::Pending,
        Refusal::Capacity,
        Refusal::Denied,
        Refusal::HubAuth,
        Refusal::HubRate,
        Refusal::TlsHandshake,
        Refusal::RecordRejected,
        Refusal::RecordRate,
        Refusal::AdmitRejected,
        Refusal::AdmitRate,
        Refusal::IrohDenied,
        Refusal::PortNoCredential,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Refusal::NotTls => "not_tls",
            Refusal::TooLarge => "too_large",
            Refusal::SlowHello => "slow_hello",
            Refusal::UnknownName => "unknown_name",
            Refusal::BadLabel => "bad_label",
            Refusal::HubOffline => "hub_offline",
            Refusal::BadTag => "bad_tag",
            Refusal::HubBusy => "hub_busy",
            Refusal::Quota => "quota",
            Refusal::PhoneRate => "phone_rate",
            Refusal::Pending => "pending",
            Refusal::Capacity => "capacity",
            Refusal::Denied => "denied",
            Refusal::HubAuth => "hub_auth",
            Refusal::HubRate => "hub_rate",
            Refusal::TlsHandshake => "tls_handshake",
            Refusal::RecordRejected => "record_rejected",
            Refusal::RecordRate => "record_rate",
            Refusal::AdmitRejected => "admit_rejected",
            Refusal::AdmitRate => "admit_rate",
            Refusal::IrohDenied => "iroh_denied",
            Refusal::PortNoCredential => "port_no_credential",
        }
    }
}

#[derive(Debug, Default)]
pub struct Counters {
    pub connections_open: AtomicI64,
    pub connections_total: AtomicU64,
    pub hubs_connected: AtomicI64,
    pub hub_sessions_total: AtomicU64,
    pub phone_active: AtomicI64,
    pub phone_total: AtomicU64,
    pub bytes_to_hub: AtomicU64,
    pub bytes_to_phone: AtomicU64,
    pub records_stored: AtomicI64,
    pub record_puts: AtomicU64,
    pub iroh_admitted: AtomicI64,
    pub iroh_denied: AtomicU64,
    pub iroh_connections: AtomicU64,
    pub api_requests: AtomicU64,
    pub acme_challenges: AtomicU64,
    /// Unix time the relay's own certificate expires; 0 if none yet.
    pub cert_not_after: AtomicI64,
    refusals: [AtomicU64; Refusal::ALL.len()],
}

impl Counters {
    pub fn refuse(&self, r: Refusal) {
        let i = Refusal::ALL.iter().position(|x| *x == r).expect("listed");
        self.refusals[i].fetch_add(1, Relaxed);
    }

    pub fn refusals(&self, r: Refusal) -> u64 {
        let i = Refusal::ALL.iter().position(|x| *x == r).expect("listed");
        self.refusals[i].load(Relaxed)
    }

    /// Prometheus text exposition (version 0.0.4).
    pub fn render(&self, version: &str, uptime_secs: u64, now_unix: i64) -> String {
        use std::fmt::Write as _;
        let mut o = String::new();
        let g = |o: &mut String, name: &str, help: &str, kind: &str, v: String| {
            let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {v}");
        };
        g(
            &mut o,
            "twr_relay_up",
            "1 while the relay runs.",
            "gauge",
            "1".into(),
        );
        let _ = writeln!(
            o,
            "# HELP twr_relay_build_info The twr version.\n# TYPE twr_relay_build_info gauge\ntwr_relay_build_info{{version=\"{version}\"}} 1"
        );
        g(
            &mut o,
            "twr_relay_uptime_seconds",
            "Seconds since start.",
            "gauge",
            uptime_secs.to_string(),
        );
        g(
            &mut o,
            "twr_relay_connections_open",
            "TCP connections open.",
            "gauge",
            self.connections_open.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_connections_total",
            "TCP connections accepted.",
            "counter",
            self.connections_total.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_hubs_connected",
            "Hub sessions open.",
            "gauge",
            self.hubs_connected.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_hub_sessions_total",
            "Hub sessions authenticated.",
            "counter",
            self.hub_sessions_total.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_phone_connections_active",
            "Phone connections passed through now.",
            "gauge",
            self.phone_active.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_phone_connections_total",
            "Phone connections passed through.",
            "counter",
            self.phone_total.load(Relaxed).to_string(),
        );
        let _ = writeln!(
            o,
            "# HELP twr_relay_bytes_total Bytes passed through for phones.\n# TYPE twr_relay_bytes_total counter\ntwr_relay_bytes_total{{direction=\"to_hub\"}} {}\ntwr_relay_bytes_total{{direction=\"to_phone\"}} {}",
            self.bytes_to_hub.load(Relaxed),
            self.bytes_to_phone.load(Relaxed)
        );
        let _ = writeln!(o, "# HELP twr_relay_refusals_total Refusals by reason.\n# TYPE twr_relay_refusals_total counter");
        for (i, r) in Refusal::ALL.iter().enumerate() {
            let _ = writeln!(
                o,
                "twr_relay_refusals_total{{reason=\"{}\"}} {}",
                r.name(),
                self.refusals[i].load(Relaxed)
            );
        }
        g(
            &mut o,
            "twr_relay_records_stored",
            "Signed records held.",
            "gauge",
            self.records_stored.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_record_puts_total",
            "Records stored.",
            "counter",
            self.record_puts.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_iroh_admitted",
            "Admitted iroh endpoints.",
            "gauge",
            self.iroh_admitted.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_iroh_denied_total",
            "iroh connections refused.",
            "counter",
            self.iroh_denied.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_iroh_connections_total",
            "iroh connections routed.",
            "counter",
            self.iroh_connections.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_api_requests_total",
            "HTTPS API requests.",
            "counter",
            self.api_requests.load(Relaxed).to_string(),
        );
        g(
            &mut o,
            "twr_relay_acme_challenges_total",
            "ACME tls-alpn-01 validations answered.",
            "counter",
            self.acme_challenges.load(Relaxed).to_string(),
        );
        let na = self.cert_not_after.load(Relaxed);
        let expiry = if na == 0 { -1 } else { na - now_unix };
        g(
            &mut o,
            "twr_relay_cert_expiry_seconds",
            "Seconds until the relay's own certificate expires; -1 if none yet.",
            "gauge",
            expiry.to_string(),
        );
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_every_reason() {
        let c = Counters::default();
        c.refuse(Refusal::BadTag);
        let text = c.render("0.0.0", 5, 100);
        assert!(text.contains("twr_relay_refusals_total{reason=\"bad_tag\"} 1"));
        assert!(text.contains("twr_relay_cert_expiry_seconds -1"));
        for r in Refusal::ALL {
            assert!(text.contains(&format!("reason=\"{}\"", r.name())));
        }
    }
}
