//! `twr relay`: run, install and check a relay (docs/relay.md, docs/relay-protocol.md).

use anyhow::{bail, Context};
use clap::Subcommand;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use twr_relay::server::{self, config::Config, install, probe};
use twr_relay::RelayUrl;

#[derive(Debug, Subcommand)]
pub enum RelayCmd {
    /// Run a relay in the foreground: phone passthrough, iroh's relay for linked runners and
    /// the record store, on one port. With --install, set it up as a systemd service instead
    /// (as root) and print the relay URL to give hubs.
    Serve {
        /// The configuration file (docs/relay.md). Default /etc/twr-relay/relay.toml, or none
        /// with the options below.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// The relay's DNS name, for example relay.example.com. Without one the relay runs
        /// without DNS, with a self-signed key that hubs pin.
        #[arg(long, value_name = "NAME")]
        hostname: Option<String>,
        /// The address phones and hubs connect to, when it isn't the hostname (always needed
        /// without one): the server's public IP.
        #[arg(long, value_name = "IP")]
        public_host: Option<String>,
        /// A short region name shown to hubs, such as `eu`.
        #[arg(long)]
        region: Option<String>,
        /// The Let's Encrypt contact, `mailto:you@example.com`.
        #[arg(long, value_name = "MAILTO")]
        acme_contact: Option<String>,
        /// Use a self-signed key even with a hostname.
        #[arg(long)]
        self_signed: bool,
        /// Where to listen (repeatable). Default [::]:443.
        #[arg(long, value_name = "ADDR")]
        listen: Vec<std::net::SocketAddr>,
        /// The state directory. Default /var/lib/twr-relay.
        #[arg(long, value_name = "DIR")]
        state_dir: Option<PathBuf>,
        /// Install a systemd service (twr-relay.service) running this relay, then start it.
        #[arg(long)]
        install: bool,
        /// With --install: enable the service without starting it.
        #[arg(long, requires = "install")]
        no_start: bool,
    },
    /// Check a running relay end to end: info, a test hub with a phone through it, the record
    /// store, iroh admission and iroh's relay. Exits 1 if any check fails.
    Probe {
        /// `relay://host[:port][#pin]`.
        url: RelayUrl,
        /// Connect to this address instead of resolving the URL's host.
        #[arg(long, value_name = "ADDR")]
        connect: Option<std::net::SocketAddr>,
        /// Print one JSON object.
        #[arg(long)]
        json: bool,
        /// Seconds per check.
        #[arg(long, default_value_t = 15)]
        timeout: u64,
        /// Keep the probe's keys in this directory between runs, as monitoring should: the
        /// relay then sees one hub and one admitted pair, within its per-address limits.
        #[arg(long, value_name = "DIR")]
        state: Option<PathBuf>,
    },
    /// Print the relay URL hubs use for this relay's configuration, with its pin when the
    /// relay's key is self-signed.
    Url {
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
}

#[allow(clippy::too_many_arguments)]
fn config_from(
    path: Option<PathBuf>,
    hostname: Option<String>,
    public_host: Option<String>,
    region: Option<String>,
    acme_contact: Option<String>,
    self_signed: bool,
    listen: Vec<std::net::SocketAddr>,
    state_dir: Option<PathBuf>,
) -> anyhow::Result<Config> {
    let default = PathBuf::from(install::CONFIG_PATH);
    let path = match path {
        Some(p) => Some(p),
        None if default.exists() && hostname.is_none() && public_host.is_none() => Some(default),
        None => None,
    };
    let mut cfg = match &path {
        Some(p) => Config::load(p)?,
        None => Config::default(),
    };
    if hostname.is_some() {
        cfg.relay.hostname = hostname;
    }
    if public_host.is_some() {
        cfg.relay.public_host = public_host;
    }
    if let Some(r) = region {
        cfg.relay.region = r;
    }
    if acme_contact.is_some() {
        cfg.tls.acme_contact = acme_contact;
    }
    if self_signed || (cfg.relay.hostname.is_none() && path.is_none()) {
        cfg.tls.mode = server::config::TlsMode::SelfSigned;
    }
    if !listen.is_empty() {
        cfg.relay.listen = listen;
    }
    if let Some(d) = state_dir {
        cfg.relay.state_dir = d;
    }
    if path.is_none() && cfg.relay.hostname.is_none() && cfg.iroh.listen.is_none() {
        cfg.iroh.listen = Some("[::]:7443".parse().unwrap());
    }
    cfg.validate().context("the relay configuration")?;
    Ok(cfg)
}

pub async fn run(cmd: RelayCmd) -> anyhow::Result<ExitCode> {
    let version = env!("CARGO_PKG_VERSION");
    match cmd {
        RelayCmd::Serve {
            config,
            hostname,
            public_host,
            region,
            acme_contact,
            self_signed,
            listen,
            state_dir,
            install: true,
            no_start,
        } => {
            if !listen.is_empty() || state_dir.is_some() {
                bail!("--install writes a config with the default listen address and state directory; edit /etc/twr-relay/relay.toml afterwards to change them");
            }
            let url = install::install(&install::InstallOptions {
                hostname,
                public_host,
                region,
                acme_contact,
                self_signed,
                no_start,
                config_path: config,
            })?;
            println!(
                "twr-relay.service is installed{}.",
                if no_start { "" } else { " and running" }
            );
            println!("Give hubs this relay URL:\n\n    {url}\n");
            println!("On a hub: set it as the relay (docs/relay.md#using-a-relay).");
            Ok(ExitCode::SUCCESS)
        }
        RelayCmd::Serve {
            config,
            hostname,
            public_host,
            region,
            acme_contact,
            self_signed,
            listen,
            state_dir,
            ..
        } => {
            // The relay's own code logs no addresses, names, tags or keys at any level. The
            // embedded iroh relay's log carries endpoint ids and peer addresses, so it is off
            // whatever RUST_LOG says (docs/relay-protocol.md §14); its counters remain.
            let mut filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
            for d in [
                "iroh_relay=off",
                "iroh_dns=off",
                "noq=off",
                "noq_proto=off",
                "tokio_rustls_acme=info",
                "hyper=off",
                "h2=off",
            ] {
                filter = filter.add_directive(d.parse().expect("a directive"));
            }
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(filter)
                .try_init();
            let cfg = config_from(
                config,
                hostname,
                public_host,
                region,
                acme_contact,
                self_signed,
                listen,
                state_dir,
            )?;
            let running = server::start(cfg, version).await?;
            eprintln!(
                "twr relay {version} listening on {:?}; relay URL {}",
                running.addrs,
                running.relay_url()
            );
            running.run_until_signal().await?;
            Ok(ExitCode::SUCCESS)
        }
        RelayCmd::Probe {
            url,
            connect,
            json,
            timeout,
            state,
        } => {
            let mut target = twr_relay::client::Target::new(url);
            target.connect = connect;
            let report = probe::probe(&probe::ProbeOptions {
                target,
                client: format!("twr {version} probe"),
                timeout: Duration::from_secs(timeout.clamp(1, 300)),
                state,
            })
            .await;
            Ok(ExitCode::from(probe::print(&report, json) as u8))
        }
        RelayCmd::Url { config } => {
            let path = config.unwrap_or_else(|| PathBuf::from(install::CONFIG_PATH));
            let cfg = Config::load(&path)?;
            let pin = match cfg.tls.mode {
                server::config::TlsMode::SelfSigned => {
                    let key = cfg.relay.state_dir.join(server::certs::SELF_KEY);
                    if !key.exists() {
                        bail!("{} doesn't exist yet: start the relay once", key.display());
                    }
                    Some(server::certs::self_signed_key(&cfg.relay.state_dir)?.1)
                }
                _ => None,
            };
            println!("{}", install::relay_url_for(&cfg, pin));
            Ok(ExitCode::SUCCESS)
        }
    }
}
