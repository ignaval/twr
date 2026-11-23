//! `twr relay serve --install`: a systemd service for self-hosters and for twr's own servers
//! (docs/relay.md).

use super::config::{Config, TlsMode};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const USER: &str = "twr-relay";
pub const CONFIG_PATH: &str = "/etc/twr-relay/relay.toml";
pub const STATE_DIR: &str = "/var/lib/twr-relay";
pub const UNIT_PATH: &str = "/etc/systemd/system/twr-relay.service";
pub const BINARY: &str = "/usr/local/bin/twr";

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub hostname: Option<String>,
    pub public_host: Option<String>,
    pub region: Option<String>,
    pub acme_contact: Option<String>,
    pub self_signed: bool,
    pub no_start: bool,
    pub config_path: Option<PathBuf>,
}

/// The config file `--install` writes when there is none.
pub fn config_text(o: &InstallOptions) -> Result<String> {
    let mut t = String::from("# twr relay configuration (docs/relay.md). Every limit not set here has its documented default.\n\n[relay]\n");
    if let Some(h) = &o.hostname {
        t += &format!("hostname = {}\n", toml_str(h));
    }
    if let Some(p) = &o.public_host {
        t += &format!("public_host = {}\n", toml_str(p));
    }
    t += &format!("region = {}\n", toml_str(o.region.as_deref().unwrap_or("default")));
    t += "listen = [\"[::]:443\"]\n";
    t += &format!("state_dir = {}\n\n[tls]\n", toml_str(STATE_DIR));
    let self_signed = o.self_signed || o.hostname.is_none();
    if self_signed {
        t += "mode = \"self-signed\"\n";
    } else {
        t += "mode = \"acme\"\n";
        match &o.acme_contact {
            Some(c) => t += &format!("acme_contact = {}\n", toml_str(c)),
            None => t += "# acme_contact = \"mailto:you@example.com\"\n",
        }
    }
    t += "\n[iroh]\nenabled = true\n";
    if o.hostname.is_none() {
        t += "# Without a DNS name iroh needs a port of its own.\nlisten = \"[::]:7443\"\n";
    }
    let cfg: Config = toml::from_str(&t).context("the generated config")?;
    cfg.validate()?;
    if self_signed && cfg.tls.mode != TlsMode::SelfSigned {
        bail!("internal: expected a self-signed config");
    }
    Ok(t)
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// The systemd unit.
pub fn unit_text(config: &Path) -> String {
    format!(
        "# Written by `twr relay serve --install` (docs/relay.md).
[Unit]
Description=twr relay
Documentation=https://github.com/ignaval/twr/blob/main/docs/relay.md
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User={USER}
Group={USER}
ExecStart={BINARY} relay serve --config {config}
Restart=always
RestartSec=2
TimeoutStopSec=20
KillSignal=SIGTERM
LimitNOFILE=65536
Environment=RUST_LOG=info
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictSUIDSGID=yes
RestrictRealtime=yes
RestrictNamespaces=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallFilter=~@privileged @resources
ReadWritePaths={STATE_DIR}
UMask=0077

[Install]
WantedBy=multi-user.target
",
        config = config.display()
    )
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed: {status}");
    }
    Ok(())
}

/// Installs and starts the service. Returns the relay URL to give hubs.
pub fn install(o: &InstallOptions) -> Result<String> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("--install sets up a system service: run it as root (sudo twr relay serve --install ...)");
    }
    let exe = std::env::current_exe().context("finding this twr")?;
    if exe != Path::new(BINARY) {
        bail!(
            "the service runs {BINARY}, and this is {}. Install twr there first (for example `sudo install -m 0755 {} {BINARY}`), then run `sudo {BINARY} relay serve --install`.",
            exe.display(),
            exe.display()
        );
    }
    let config = o.config_path.clone().unwrap_or_else(|| PathBuf::from(CONFIG_PATH));

    // The user.
    let exists = Command::new("id").arg("-u").arg(USER).output().map(|o| o.status.success()).unwrap_or(false);
    if !exists {
        run(Command::new("useradd").args(["--system", "--no-create-home", "--home-dir", STATE_DIR, "--shell", "/usr/sbin/nologin", USER]))?;
    }

    // The config, if there is none.
    if config.exists() {
        eprintln!("keeping the existing {}", config.display());
    } else {
        let text = config_text(o)?;
        if let Some(dir) = config.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&config, text)?;
        eprintln!("wrote {}", config.display());
    }
    let cfg = Config::load(&config)?;

    // The state directory, owned by the service user.
    std::fs::create_dir_all(&cfg.relay.state_dir)?;
    run(Command::new("chown").arg(format!("{USER}:{USER}")).arg(&cfg.relay.state_dir))?;
    run(Command::new("chmod").arg("0700").arg(&cfg.relay.state_dir))?;

    // A self-signed key now, so the URL with its pin can be printed.
    let mut pin = None;
    if cfg.tls.mode == TlsMode::SelfSigned {
        let (_, p) = super::certs::self_signed_key(&cfg.relay.state_dir)?;
        run(Command::new("chown").arg(format!("{USER}:{USER}")).arg(cfg.relay.state_dir.join(super::certs::SELF_KEY)))?;
        pin = Some(p);
    }

    std::fs::write(UNIT_PATH, unit_text(&config))?;
    run(Command::new("systemctl").arg("daemon-reload"))?;
    if o.no_start {
        run(Command::new("systemctl").args(["enable", "twr-relay.service"]))?;
    } else {
        run(Command::new("systemctl").args(["enable", "--now", "twr-relay.service"]))?;
        // A changed binary or config needs a restart, not just a start.
        run(Command::new("systemctl").args(["restart", "twr-relay.service"]))?;
    }
    Ok(relay_url_for(&cfg, pin))
}

/// The URL hubs use for a relay with this configuration.
pub fn relay_url_for(cfg: &Config, pin: Option<[u8; 32]>) -> String {
    let host = cfg.relay.hostname.clone().unwrap_or_else(|| cfg.public_host());
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    let port = cfg.public_port();
    let mut url = match port {
        443 => format!("relay://{host}"),
        p => format!("relay://{host}:{p}"),
    };
    if let Some(p) = pin {
        url += "#";
        url += &crate::names::pin_text(&p);
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_validate() {
        let named = config_text(&InstallOptions {
            hostname: Some("relay.example.com".into()),
            acme_contact: Some("mailto:ops@example.com".into()),
            region: Some("eu".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(named.contains("mode = \"acme\""));
        let bare = config_text(&InstallOptions { public_host: Some("203.0.113.9".into()), ..Default::default() }).unwrap();
        assert!(bare.contains("self-signed"));
        let cfg: Config = toml::from_str(&bare).unwrap();
        assert_eq!(relay_url_for(&cfg, Some([0; 32])), format!("relay://203.0.113.9#{}", crate::names::pin_text(&[0; 32])));
        assert!(config_text(&InstallOptions::default()).is_err(), "needs a hostname or a public host");
    }

    #[test]
    fn unit_runs_unprivileged() {
        let u = unit_text(Path::new(CONFIG_PATH));
        assert!(u.contains("User=twr-relay"));
        assert!(u.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE"));
        assert!(u.contains("ExecStart=/usr/local/bin/twr relay serve --config /etc/twr-relay/relay.toml"));
    }
}
