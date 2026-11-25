//! Pairing phones from this machine (`twr phone`, the TUI's `/phone`) and starting the pinned
//! gateway with a local hub. The gateway runs on its own, outliving whatever started it, as the
//! runner service does; a hub that starts runs it only when phones are paired with it.
use crate::gateway::{self, settings::Settings};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};
use twr_core::devices::{Auth, InvitationState};

/// `TWR_GATEWAY_AUTOSTART=off`: a hub that starts never starts the gateway.
pub const AUTOSTART_ENV: &str = "TWR_GATEWAY_AUTOSTART";
/// The gateway's log, in its directory.
pub const LOG_FILE: &str = "gateway.log";

/// The gateway's directory, `mobile/` beside the config file: its identity, settings and the
/// paired devices.
pub fn gateway_dir() -> Result<PathBuf> {
    let paths = twr_core::paths::Paths::from_env_in(Some("default"))?;
    Ok(paths.config_file.with_file_name("mobile"))
}

/// This executable, to start the gateway with: where it was started from, or the `twr` an
/// update put in its place (Linux names a replaced executable `<path> (deleted)`).
pub fn this_program() -> Result<PathBuf> {
    let program = std::env::current_exe().context("finding this twr")?;
    if program.exists() {
        return Ok(program);
    }
    let replaced = program
        .to_str()
        .and_then(|p| p.strip_suffix(" (deleted)"))
        .map(PathBuf::from)
        .filter(|p| p.exists());
    replaced.with_context(|| format!("{} is gone", program.display()))
}

/// What a hub that starts does about the gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autostart {
    Start,
    /// Nothing, and why.
    Dormant(&'static str),
}

/// What a starting hub does about the gateway: starts it when nothing forbids it
/// (`forbidden`: a test's hub, [`AUTOSTART_ENV`]), `twr phone` set it up (`settings`, with
/// autostart on, which only `twr phone` turns on and `twr phone off` or `--stop-gateway`
/// off), none runs (`running`), and a phone is paired or an invitation open. It then listens
/// where the settings say.
pub fn autostart_decision(
    forbidden: bool,
    settings: Option<&Settings>,
    running: bool,
    devices: usize,
    invitation_open: bool,
) -> Autostart {
    if forbidden {
        Autostart::Dormant("autostart is off here")
    } else if !settings.is_some_and(|s| s.autostart) {
        Autostart::Dormant("not set up, or turned off with `twr phone off`")
    } else if running {
        Autostart::Dormant("a gateway runs")
    } else if devices == 0 && !invitation_open {
        Autostart::Dormant("no phone is paired")
    } else {
        Autostart::Start
    }
}

/// Whether `value` of [`AUTOSTART_ENV`] forbids autostart.
pub fn autostart_off(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "off" || v == "0")
}

/// After a local hub on `socket` answers, starts the gateway with `program` (this `twr`)
/// when [`autostart_decision`] says so. `forbidden` is the caller's: a test's hub.
pub async fn autostart(program: &Path, socket: &Path, forbidden: bool) -> Result<Autostart> {
    let forbidden =
        forbidden || cfg!(test) || autostart_off(std::env::var_os(AUTOSTART_ENV).as_deref());
    if forbidden {
        return Ok(Autostart::Dormant("autostart is off here"));
    }
    // The gateway starts the hub it serves if none answers: wait for this one.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while tokio::net::UnixStream::connect(socket).await.is_err() {
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the hub did not answer"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let dir = gateway_dir()?;
    let settings = Settings::load(&dir)?;
    // Nothing set up: nothing read, nothing made.
    if !settings.as_ref().is_some_and(|s| s.autostart) {
        return Ok(autostart_decision(
            false,
            settings.as_ref(),
            false,
            0,
            false,
        ));
    }
    let running = crate::remote::gateway_lifecycle::info(&dir).await?["running"] == true;
    let (devices, open) = if dir.join("devices.json").exists() {
        Auth::open(&dir)?.summary()?
    } else {
        (0, false)
    };
    let decision = autostart_decision(false, settings.as_ref(), running, devices, open);
    if decision == Autostart::Start {
        start_detached(program, &dir)?;
    }
    Ok(decision)
}

/// Starts `program gateway` (pinned mode, its saved settings) in a session of its own, outside
/// this process's tree: it outlives whatever started it. Its output goes to [`LOG_FILE`].
pub fn start_detached(program: &Path, dir: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    crate::remote::gateway_lifecycle::private_dir(dir)?;
    let log_path = dir.join(LOG_FILE);
    let log = twr_core::paths::open_private_append(&log_path)?;
    let mut command = std::process::Command::new("sh");
    // A short-lived shell forks the gateway then exits, as for the runner service.
    command
        .args(["-c", "\"$@\" &", "twr-gateway-launch"])
        .arg(program)
        .arg("gateway")
        .arg("--log-file")
        .arg(&log_path)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Nothing of an agent's, a device's or a workspace's: the gateway is the user's own.
        .env_remove("TWR_AGENT")
        .env_remove("TWR_THREAD")
        .env_remove("TWR_RUNTIME_GEN")
        .env_remove("TWR_DEVICE_TOKEN")
        .env_remove("TWR_MOBILE_TRANSPORT")
        .env_remove("TWR_WORKSPACE")
        .env_remove(twr_core::paths::WORKSPACE_ID_ENV)
        .env_remove("TWR_ORCH_TOKEN");
    twr_core::fork::prepare_fork();
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
        command.pre_exec(twr_core::fork::close_inherited_on_exec);
    }
    let mut child = command.spawn().context("starting the gateway")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// The gateway's record once one runs, and whether this started it: the running one, else one
/// `program` starts (detached), waited for. Errors name its log.
pub async fn ensure_running(program: &Path, dir: &Path) -> Result<(Value, bool)> {
    let info = crate::remote::gateway_lifecycle::info(dir).await?;
    if info["running"] == true {
        return Ok((info, false));
    }
    start_detached(program, dir)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(info) = crate::remote::gateway_lifecycle::info(dir).await {
            if info["running"] == true {
                return Ok((info, true));
            }
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the gateway did not start within a minute; see {}",
            dir.join(LOG_FILE).display()
        );
    }
}

/// What `twr phone` saves as the pinned gateway's `listen` when one runs on `running`:
/// nothing (the default) when it listens on [`gateway::ALL_ADDRESSES`], else exactly those
/// addresses, so a hub that starts it later listens where it does now.
pub fn listen_to_save(running: &[std::net::SocketAddr]) -> Vec<std::net::SocketAddr> {
    if running == gateway::ALL_ADDRESSES {
        Vec::new()
    } else {
        running.to_vec()
    }
}

/// `twr phone`: saves in `dir`'s settings where the pinned gateway whose record is `info`
/// listens ([`listen_to_save`]), when it is a pinned one and that changed them.
pub fn remember_listen(dir: &Path, info: &Value) -> Result<()> {
    let running = gateway::listeners_of(info);
    if info["mode"] != "pinned" || running.is_empty() {
        return Ok(());
    }
    let saved = Settings::load(dir)?;
    let mut settings = saved.clone().unwrap_or_default();
    settings.listen = listen_to_save(&running);
    if saved.as_ref() != Some(&settings) {
        settings.save(dir)?;
    }
    Ok(())
}

/// The scopes of an access level: `read-only`, `operate` (the default), `terminal` or `admin`
/// (which includes terminal).
pub fn scopes_of(level: &str) -> Option<Vec<&'static str>> {
    Some(match level {
        "read-only" | "read" => vec!["read"],
        "operate" | "" => vec!["read", "operate"],
        "terminal" => vec!["read", "operate", "terminal"],
        "admin" => vec!["read", "operate", "terminal", "admin"],
        _ => return None,
    })
}

/// An invitation open on the pinned gateway that runs in `dir`: its `--pair --json` line, and
/// the devices paired before it (to tell which one pairs with it).
pub struct Opened {
    pub line: Value,
    pub known: HashSet<String>,
}

/// Opens an invitation for `scopes` on the gateway of `dir`, which must run in pinned mode.
pub async fn open_invitation(dir: &Path, scopes: &[&str]) -> Result<Opened> {
    let pinned = gateway::running_pinned(dir).await?.context(
        "the gateway that runs serves a certificate (the desktop app's): pair from the desktop \
         app, or stop that gateway first",
    )?;
    let known = paired_ids(&Auth::open(dir)?)?;
    let mut owner = gateway::owner_client().await?;
    let line = gateway::begin_invitation(&mut owner, scopes, dir, Some(&pinned)).await?;
    Ok(Opened { line, known })
}

fn paired_ids(auth: &Auth) -> Result<HashSet<String>> {
    Ok(auth
        .devices()?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["id"].as_str().map(String::from))
        .collect())
}

/// How an invitation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A device paired with it: its record (`id`, `name`, `scopes`, `created`).
    Paired(Value),
    /// It expired, ran out of attempts or was cancelled elsewhere.
    Expired,
    /// A newer invitation replaced it.
    Replaced,
    /// `stop` ended the wait, and the invitation was cancelled.
    Cancelled,
}

impl Outcome {
    /// The JSON line `twr phone --json` ends with.
    pub fn json(&self) -> Value {
        match self {
            Outcome::Paired(device) => json!({"paired": device}),
            Outcome::Expired => json!({"expired": true}),
            Outcome::Replaced => json!({"replaced": true}),
            Outcome::Cancelled => json!({"cancelled": true}),
        }
    }
}

/// Waits until the invitation of `code` ends, or `stop` does (which cancels it).
pub async fn wait(
    dir: &Path,
    code: &str,
    known: &HashSet<String>,
    stop: impl std::future::Future<Output = ()>,
) -> Result<Outcome> {
    let auth = Auth::open(dir)?;
    tokio::pin!(stop);
    let mut tick = tokio::time::interval(Duration::from_millis(300));
    loop {
        tokio::select! {
            _ = &mut stop => {
                auth.cancel_pairing(code)?;
                return Ok(Outcome::Cancelled);
            }
            _ = tick.tick() => {}
        }
        match auth.invitation(code)? {
            InvitationState::Open => continue,
            InvitationState::Replaced => return Ok(Outcome::Replaced),
            InvitationState::Gone => {}
        }
        let devices = auth.devices()?;
        let paired = devices
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["id"].as_str().is_some_and(|id| !known.contains(id)));
        return Ok(match paired {
            Some(device) => Outcome::Paired(device.clone()),
            None => {
                // Expired or used up: nothing of it (its proof key) stays behind.
                auth.cancel_pairing(code)?;
                Outcome::Expired
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hub_starts_the_gateway_only_when_phones_are_paired_and_nothing_forbids_it() {
        let on = Settings {
            autostart: true,
            ..Default::default()
        };
        let off = Settings::default();
        assert_eq!(
            autostart_decision(false, Some(&on), false, 1, false),
            Autostart::Start
        );
        assert_eq!(
            autostart_decision(false, Some(&on), false, 0, true),
            Autostart::Start
        );
        for (forbidden, settings, running, devices, open) in [
            (true, Some(&on), false, 1, false),
            (false, None, false, 1, false),
            (false, Some(&off), false, 1, true),
            (false, Some(&on), true, 1, false),
            (false, Some(&on), false, 0, false),
        ] {
            assert!(
                matches!(
                    autostart_decision(forbidden, settings, running, devices, open),
                    Autostart::Dormant(_)
                ),
                "{forbidden} {settings:?} {running} {devices} {open}"
            );
        }
        assert!(autostart_off(Some("off".as_ref())));
        assert!(autostart_off(Some("0".as_ref())));
        assert!(!autostart_off(Some("on".as_ref())));
        assert!(!autostart_off(None));
    }

    /// `twr phone` saves where a pinned gateway listens, the default as nothing; a gateway in
    /// another mode saves nothing.
    #[test]
    fn twr_phone_saves_where_the_pinned_gateway_listens() {
        use std::os::unix::fs::PermissionsExt;
        let all = gateway::ALL_ADDRESSES;
        assert!(listen_to_save(&all).is_empty());
        let narrow: Vec<std::net::SocketAddr> = vec!["192.168.1.5:47321".parse().unwrap()];
        assert_eq!(listen_to_save(&narrow), narrow);
        assert_eq!(listen_to_save(&all[..1]), all[..1]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let on = Settings {
            autostart: true,
            ..Default::default()
        };
        on.save(dir.path()).unwrap();
        let certificate = json!({"mode": "certificate", "listen": "127.0.0.1:47321"});
        remember_listen(dir.path(), &certificate).unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), Some(on.clone()));
        let pinned = json!({"mode": "pinned", "listen": "192.168.1.5:47321",
            "listens": ["192.168.1.5:47321"]});
        remember_listen(dir.path(), &pinned).unwrap();
        let saved = Settings::load(dir.path()).unwrap().unwrap();
        assert_eq!(saved.listen, narrow);
        assert!(saved.autostart);
        let everywhere = json!({"mode": "pinned", "listen": "0.0.0.0:47321",
            "listens": ["0.0.0.0:47321", "[::]:47321"]});
        remember_listen(dir.path(), &everywhere).unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), Some(on));
    }

    #[test]
    fn access_levels_name_their_scopes() {
        assert_eq!(scopes_of("read-only").unwrap(), ["read"]);
        assert_eq!(scopes_of("operate").unwrap(), ["read", "operate"]);
        assert_eq!(
            scopes_of("terminal").unwrap(),
            ["read", "operate", "terminal"]
        );
        assert_eq!(
            scopes_of("admin").unwrap(),
            ["read", "operate", "terminal", "admin"]
        );
        assert!(scopes_of("root").is_none());
    }

    /// The wait ends with the device that paired, with an expiry, with a newer invitation, and
    /// with `stop`, which cancels the invitation.
    #[tokio::test]
    async fn the_wait_tells_how_an_invitation_ended() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        let code = auth.begin(vec!["read".into()]).unwrap();
        let known = paired_ids(&auth).unwrap();
        let pairing = {
            let code = code.clone();
            let dir = dir.path().to_owned();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Auth::open(&dir).unwrap().pair(&code, "Phone").unwrap();
            })
        };
        let outcome = wait(dir.path(), &code, &known, std::future::pending()).await;
        pairing.await.unwrap();
        let Outcome::Paired(device) = outcome.unwrap() else {
            panic!("not paired")
        };
        assert_eq!(device["name"], "Phone");
        let known = paired_ids(&auth).unwrap();
        let first = auth.begin(vec!["read".into()]).unwrap();
        auth.begin(vec!["read".into()]).unwrap();
        let outcome = wait(dir.path(), &first, &known, std::future::pending()).await;
        assert_eq!(outcome.unwrap(), Outcome::Replaced);
        let code = auth.begin(vec!["read".into()]).unwrap();
        let stop = tokio::time::sleep(Duration::from_millis(100));
        assert_eq!(
            wait(dir.path(), &code, &known, stop).await.unwrap(),
            Outcome::Cancelled
        );
        assert_eq!(auth.invitation(&code).unwrap(), InvitationState::Gone);
        let code = auth.begin(vec!["read".into()]).unwrap();
        auth.cancel_pairing(&code).unwrap();
        let outcome = wait(dir.path(), &code, &known, std::future::pending()).await;
        assert_eq!(outcome.unwrap(), Outcome::Expired);
        assert_eq!(Outcome::Expired.json(), json!({"expired": true}));
        // Expired: the wait says so, and the invitation's proof key is gone from the store.
        let code = auth.begin(vec!["read".into()]).unwrap();
        let path = dir.path().join("devices.json");
        let key = hex::encode(twr_core::phone_pairing::proof_key(&code).unwrap());
        let mut db: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(db.to_string().contains(&key));
        db["pairing"]["expires"] = json!(twr_core::devices::now() - 1);
        std::fs::write(&path, db.to_string()).unwrap();
        let outcome = wait(dir.path(), &code, &known, std::future::pending()).await;
        assert_eq!(outcome.unwrap(), Outcome::Expired);
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(!stored.contains(&key) && !stored.contains("\"pairing\":{"));
    }
}
