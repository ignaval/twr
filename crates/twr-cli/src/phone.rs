//! `twr phone`: pair a phone with this machine's hubs through the pinned gateway, list and
//! revoke paired phones, turn the gateway off, and make it a new key. The user's alone: an
//! agent is refused every one of them.
use anyhow::{bail, Context};
use clap::Subcommand;
use serde_json::{json, Value};
use std::process::ExitCode;
use twr_app::{gateway, phone};

#[derive(Debug, Subcommand)]
pub enum PhoneCmd {
    /// List the paired phones.
    Devices {
        /// One JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Revoke a paired phone (its id, or the start of it, from `twr phone devices`): it is
    /// disconnected at once and must pair again.
    Revoke { device: String },
    /// Stop the gateway, and keep a hub that starts from starting it. `twr phone` turns it
    /// back on.
    Off,
    /// Make the gateway a new key (and a new iroh key). Every paired phone pinned the old one:
    /// all of them are revoked and must pair again.
    Rekey {
        /// Don't ask.
        #[arg(long)]
        yes: bool,
    },
}

/// The access an invitation grants (`--read-only`, the default, `--terminal`, `--admin`).
pub fn level(read_only: bool, terminal: bool, admin: bool) -> &'static str {
    match (read_only, terminal, admin) {
        (true, ..) => "read-only",
        (_, _, true) => "admin",
        (_, true, _) => "terminal",
        _ => "operate",
    }
}

fn describe(level: &str) -> &'static str {
    match level {
        "read-only" => "read only",
        "terminal" => "read, operate and terminal",
        "admin" => "read, operate, terminal and admin",
        _ => "read and operate",
    }
}

/// `twr phone` without a subcommand: an invitation, shown on this terminal (or with `json`
/// as JSON lines), and the wait for a phone to take it.
pub async fn pair(level: &str, address: Option<&str>, json: bool) -> anyhow::Result<ExitCode> {
    crate::refuse_agent_action("pair phones")?;
    use std::io::IsTerminal;
    if !json && !std::io::stdout().is_terminal() {
        bail!(
            "twr phone shows a pairing code, and only on a terminal: run it in one, or give \
             --json for its JSON lines"
        );
    }
    let scopes = phone::scopes_of(level).context("unknown access level")?;
    let dir = phone::gateway_dir()?;
    // `twr phone` turns autostart (back) on, and saves `--address`.
    let saved = gateway::settings::Settings::load(&dir)?;
    let mut settings = saved.clone().unwrap_or_default();
    settings.autostart = true;
    if let Some(address) = address {
        settings.set_address(address)?;
    }
    if saved.as_ref() != Some(&settings) {
        settings.save(&dir)?;
    }
    let program = phone::this_program()?;
    let started = phone::ensure_running(&program, &dir).await?.1;
    if started && !json {
        outln!("Started the phone gateway: it runs on its own now.");
    }
    let opened = phone::open_invitation(&dir, &scopes).await?;
    let line = &opened.line;
    let code = line["code"].as_str().context("no code")?.to_owned();
    if json {
        println!("{line}");
    } else {
        show(line, level);
    }
    let stop = async {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut term), Ok(mut hangup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hangup.recv() => {}
        }
    };
    let outcome = phone::wait(&dir, &code, &opened.known, stop).await?;
    if json {
        println!("{}", outcome.json());
    } else {
        match &outcome {
            phone::Outcome::Paired(device) => outln!(
                "\nPaired {} ({}).",
                device["name"].as_str().unwrap_or("a phone"),
                describe(level)
            ),
            phone::Outcome::Expired => {
                outln!("\nThe code expired or was used up. `twr phone` makes a new one.")
            }
            phone::Outcome::Replaced => outln!("\nA newer invitation replaced this one."),
            phone::Outcome::Cancelled => outln!("\nCancelled: the code no longer works."),
        }
    }
    Ok(match outcome {
        phone::Outcome::Paired(_) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

/// The width of the terminal on stdout, in columns.
fn columns() -> Option<usize> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ fills `size`.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0).then_some(size.ws_col as usize)
}

/// The routes of an invitation as addresses to type: `host:port`.
fn addresses(line: &Value) -> Vec<String> {
    line["invitation"]["routes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str()?.strip_prefix("tls:").map(String::from))
        .collect()
}

/// The invitation, for this terminal: its QR code when the terminal is wide enough (of the
/// invitation's JSON without `iroh` and `scopes`, [`twr_tui::qr::invitation_text`]), the link,
/// the code, the addresses and the expiry.
fn show(line: &Value, level: &str) {
    let link = line["link"].as_str().unwrap_or("");
    let qr = twr_tui::qr::invitation_text(&line["invitation"]);
    let columns = columns().unwrap_or(80);
    outln!();
    if twr_tui::qr::fits(&qr, columns) {
        let lines = twr_tui::qr::half_blocks(&qr).unwrap_or_default();
        print!("{}", twr_tui::qr::ansi(&lines));
        outln!("\nScan it in the twr app: Pair a hub › Scan QR code.");
    } else {
        outln!(
            "This terminal is too narrow for the QR code ({} columns would do). Open the link \
             on the phone, or type the code and one of the addresses in the twr app.",
            twr_tui::qr::width(&qr).unwrap_or(0)
        );
    }
    outln!("\nLink: {link}");
    outln!(
        "Code: {}",
        twr_core::phone_pairing::display_code(line["code"].as_str().unwrap_or(""))
    );
    let addresses = addresses(line);
    if !addresses.is_empty() {
        outln!("Addresses, to type with the code: {}", addresses.join("  "));
    }
    outln!("Access: {}", describe(level));
    let expires = line["expires_at"].as_i64().unwrap_or(0);
    let at = chrono::DateTime::from_timestamp(expires, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default();
    outln!(
        "The code works once, until {at} (five minutes). Waiting for the phone; Ctrl-C cancels."
    );
}

/// `twr phone devices|revoke|off|rekey`.
pub async fn command(cmd: PhoneCmd) -> anyhow::Result<ExitCode> {
    crate::refuse_agent_action("manage paired phones")?;
    let dir = phone::gateway_dir()?;
    match cmd {
        PhoneCmd::Devices { json: as_json } => {
            let devices = devices(&dir)?;
            if as_json {
                println!("{devices}");
            } else if devices.as_array().is_none_or(Vec::is_empty) {
                outln!("No phone is paired. `twr phone` pairs one.");
            } else {
                for d in devices.as_array().into_iter().flatten() {
                    let created = d["created"].as_i64().unwrap_or(0);
                    let when = chrono::DateTime::from_timestamp(created, 0)
                        .map(|t| {
                            t.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d")
                                .to_string()
                        })
                        .unwrap_or_default();
                    let scopes: Vec<&str> = d["scopes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    outln!(
                        "{}  {}  {}  paired {when}",
                        d["id"].as_str().unwrap_or(""),
                        d["name"].as_str().unwrap_or(""),
                        scopes.join(",")
                    );
                }
            }
        }
        PhoneCmd::Revoke { device } => {
            let devices = devices(&dir)?;
            let matching: Vec<&Value> = devices
                .as_array()
                .into_iter()
                .flatten()
                .filter(|d| {
                    d["id"].as_str().is_some_and(|id| {
                        id == device || (device.len() >= 4 && id.starts_with(&device))
                    })
                })
                .collect();
            let found = match matching.as_slice() {
                [one] => *one,
                [] => bail!("no paired phone has the id {device} (`twr phone devices` lists them)"),
                _ => bail!("several paired phones' ids start with {device}: give more of it"),
            };
            let id = found["id"].as_str().unwrap_or("");
            twr_core::devices::Auth::open(&dir)?.revoke(id)?;
            outln!("Revoked {} ({id}).", found["name"].as_str().unwrap_or(""));
        }
        PhoneCmd::Off => {
            let mut settings = gateway::settings::Settings::load(&dir)?.unwrap_or_default();
            settings.autostart = false;
            settings.save(&dir)?;
            match stop_pinned(&dir).await? {
                Stopped::Pinned => outln!("Stopped the phone gateway."),
                Stopped::NotRunning => outln!("The phone gateway is not running."),
                Stopped::Other => outln!(
                    "The gateway that runs is the desktop app's (it serves a certificate): stop \
                     it from the desktop app."
                ),
            }
            outln!("A hub that starts won't start it. `twr phone` turns it back on.");
        }
        PhoneCmd::Rekey { yes } => {
            let count = devices(&dir)?.as_array().map_or(0, Vec::len);
            outln!(
                "This makes the phone gateway a new key. Phones pin the key they paired with, so \
                 every paired phone ({count}) is revoked and must pair again."
            );
            if !yes && !confirm("Make a new key? [y/N] ")? {
                outln!("Nothing changed.");
                return Ok(ExitCode::FAILURE);
            }
            let stopped = stop_pinned(&dir).await?;
            anyhow::ensure!(
                stopped != Stopped::Other,
                "the gateway that runs is the desktop app's: stop it from the desktop app first"
            );
            let identity = gateway::identity::Identity::rekey(&dir)?;
            let revoked = twr_core::devices::Auth::open(&dir)?.revoke_all()?;
            outln!(
                "New key made; {revoked} phone(s) revoked. New pin: {}",
                identity.pin
            );
            if stopped == Stopped::Pinned {
                let program = phone::this_program()?;
                phone::ensure_running(&program, &dir).await?;
                outln!("The gateway runs again with it. `twr phone` pairs a phone.");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn devices(dir: &std::path::Path) -> anyhow::Result<Value> {
    if !dir.join("devices.json").exists() {
        return Ok(json!([]));
    }
    twr_core::devices::Auth::open(dir)?.devices()
}

#[derive(Debug, PartialEq, Eq)]
enum Stopped {
    Pinned,
    NotRunning,
    /// A gateway in another mode runs (the desktop app's): left alone.
    Other,
}

/// Stops the pinned gateway of `dir` if it runs.
async fn stop_pinned(dir: &std::path::Path) -> anyhow::Result<Stopped> {
    let info = twr_app::remote::gateway_lifecycle::info(dir).await?;
    if info["running"] != true {
        return Ok(Stopped::NotRunning);
    }
    if info["mode"] != "pinned" {
        return Ok(Stopped::Other);
    }
    let instance = info["instance"].as_str().context("gateway instance")?;
    twr_app::remote::gateway_lifecycle::stop(dir, instance).await?;
    Ok(Stopped::Pinned)
}

/// A y/N answer on this terminal.
fn confirm(prompt: &str) -> anyhow::Result<bool> {
    use std::io::{IsTerminal, Write};
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "no terminal to confirm on: give --yes"
    );
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_flags_pick_one_level() {
        assert_eq!(level(false, false, false), "operate");
        assert_eq!(level(true, false, false), "read-only");
        assert_eq!(level(false, true, false), "terminal");
        assert_eq!(level(false, true, true), "admin");
        assert_eq!(level(false, false, true), "admin");
    }

    #[test]
    fn invitation_routes_read_as_addresses() {
        let line = json!({"invitation":{"routes":["tls:192.168.1.20:47321","tls:[2001:db8::5]:47321","relay:x"]}});
        assert_eq!(
            addresses(&line),
            ["192.168.1.20:47321", "[2001:db8::5]:47321"]
        );
    }
}
