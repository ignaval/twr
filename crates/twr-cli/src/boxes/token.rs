//! The hub's side of dial-in boxes (docs/runners.md, "Dial-in boxes"): `twr box add <name>
//! --dial-in --hub <url>` writes a token box with a new token to `boxes.toml`, adds it to the
//! running hub and prints the one command to run on the box; `twr box token <name>` prints a
//! token box's token, replaces it (`--rotate`) or has the running hub read it again
//! (`--reload`); and `twr box remove` of a token box has the running hub stop accepting its
//! runner ([`removal_lines`]). The hub takes a token box's token live (`box.token`): only ever
//! the files' token, or none, and a change drops the box's connection.
//!
//! The token is printed by `add` and `--rotate` only when stdout is a terminal (never into a
//! redirected log or a job's output), and by `twr box token` on request, to a pipe too (`ssh
//! me@hub twr box token gpu | twr runner install-token gpu --yes`). Agents are refused, as
//! for the other box commands: not a boundary (an agent can read `boxes.toml`), but it keeps
//! the token out of agent transcripts.

use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use anyhow::bail;
use serde_json::{json, Value};
use twr_core::client::Client;
use twr_core::config::{BoxConfig, BoxTransport, BoxesFile, Config};
use twr_core::paths::{sh_word, Paths};
use twr_core::proto::{codes, RpcError};

use super::{
    ask_on_stderr, check_merged, confirm, connect, consent, default_workspace_only, dev_commit,
    find_box, no_hub, online_note, project_hints, refuse_agent_action, str_of, wait_for_hub,
    with_added_box, workspace_hint, Consent, HubBuild, ONLINE_POLL,
};
use crate::dial_in::{agent_label, unit_name};

/// How long `twr box add --dial-in` waits, on a terminal, for the box's runner to connect.
const DIAL_IN_WAIT: Duration = Duration::from_secs(600);

/// The port a hub's listener is set to in the examples, when the config has none.
const EXAMPLE_PORT: &str = "7878";

/// `twr box add <name> --dial-in --hub <url> [--ca <pem>]` (see [`super::BoxCmd::Add`]).
pub(super) async fn add(
    config: &Config,
    paths: &Paths,
    socket: &Path,
    name: &str,
    hub: Option<&str>,
    ca: Option<&str>,
    yes: bool,
) -> anyhow::Result<()> {
    refuse_agent_action("add boxes")?;
    default_workspace_only(paths, "add")?;
    twr_core::runner::validate_box_name(name)?;
    if name == twr_hub::hub::LOCAL_BOX {
        bail!("box name local is the hub's own box: name the remote box otherwise");
    }
    // Nothing is written, nor any hub started, without a listener to connect to.
    let Some(listen) = config.hub.listen.as_deref() else {
        bail!("{}", listener_off(&paths.config_file));
    };
    let url = dial_in_url(config, name, hub, ca)?;
    let file = paths.boxes_file();
    if config.boxes.contains_key(name) {
        bail!("there is a box {name} already (`twr box status {name}` shows it)");
    }
    let token = twr_core::runner::new_token()?;
    let cfg = token_box(&token);
    // Checked now so a name the files refuse fails before anything else.
    with_added_box(&paths.config_file, &file, name, cfg.clone())?;
    let mut ask = ask_on_stderr;
    let consent = consent(yes, &mut ask);
    if matches!(consent, Consent::NoTerminal) {
        bail!("not adding box {name} without confirmation (no terminal to ask on): pass --yes");
    }
    let running = HubBuild::of_running(socket).await;
    if running.box_entry(name).is_some() {
        bail!(
            "the running hub still has a box {name} (removed from the files since it started): \
             restart it first (`twr{w} hub stop`, then `twr{w}`)",
            w = workspace_hint()
        );
    }
    if let Some(why) = &running.unknown {
        errln!("twr: warning: the running hub did not say which boxes it has ({why})");
    }
    outln!("Adding dial-in box {name}:");
    for l in add_lines(name, &url, listen, &file, &running.version) {
        outln!("{l}");
    }
    if !confirm(
        consent,
        &format!("Add box {name}? [y/N] "),
        &format!("not adding box {name} without confirmation"),
    )? {
        outln!("box {name} not added");
        return Ok(());
    }

    {
        let _lock = BoxesFile::lock(&file)?;
        let boxes = with_added_box(&paths.config_file, &file, name, cfg)?;
        boxes.save(&file)?;
    }
    outln!("added [boxes.{name}] to {}", file.display());
    wait_for_hub(socket, paths).await?;
    let mut c = connect(socket).await?;
    let added = c.call("box.add", json!({ "name": name })).await;
    let added = match added {
        Ok(v) => v,
        Err(e) => bail!(
            "box {name} is in {}, but the hub did not add it: {e:#}",
            file.display()
        ),
    };
    if added["already"] == true {
        outln!("the hub has box {name} from {} already", file.display());
    }
    if added["listening"] == false {
        outln!(
            "the running hub does not listen for runners yet: restart it (`twr hub stop`, then \
             `twr`), and it listens on {listen}"
        );
    }
    let terminal = std::io::stdout().is_terminal();
    let command = box_command(name, Some(&url), ca);
    for l in token_lines(name, &command, &token, terminal) {
        outln!("{l}");
    }
    if terminal {
        wait(&mut c, config, name, &running.version).await?;
    }
    Ok(())
}

/// A token box with `token`, as `twr box add --dial-in` writes it.
fn token_box(token: &str) -> BoxConfig {
    BoxConfig {
        transport: Some(BoxTransport::Token),
        token: Some(token.to_string()),
        ..BoxConfig::default()
    }
}

/// Why `twr box add --dial-in` refuses a config without `[hub] listen` (`config_file`), and
/// how to turn the listener on and reach it from the box.
fn listener_off(config_file: &Path) -> String {
    format!(
        "a dial-in box connects to the hub's runner listener, which is off. Add to {}:\n  \
         [hub]\n  listen = \"127.0.0.1:{EXAMPLE_PORT}\"\nrestart the hub (`twr hub stop`, then \
         `twr`), and make it reachable from the box: {}. docs/runners.md#dial-in-boxes",
        config_file.display(),
        reach_hints(&format!("127.0.0.1:{EXAMPLE_PORT}"), false)
    )
}

/// How a box reaches a listener on `listen` (`host:port`): with `tls` (`[hub.tls]`), wss:// to
/// a name in its certificate; else Tailscale's TLS in front of it, or an ssh tunnel from the
/// box to it.
fn reach_hints(listen: &str, tls: bool) -> String {
    let port = listen
        .rsplit_once(':')
        .map_or(EXAMPLE_PORT, |(_, p)| p)
        .to_string();
    if tls {
        return format!("--hub wss://<a name in the hub's certificate>:{port}");
    }
    // The tunnel's far end: the listener, or loopback for an address that listens on all.
    let target = match listen.rsplit_once(':') {
        Some(("0.0.0.0" | "[::]" | "", _)) | None => format!("127.0.0.1:{port}"),
        Some(_) => listen.to_string(),
    };
    format!(
        "Tailscale (`tailscale serve --bg --tls-terminated-tcp={port} tcp://{target}` here, then \
         --hub wss://<this machine>.<tailnet>.ts.net:{port}) or an ssh tunnel from the box (`ssh \
         -N -o ExitOnForwardFailure=yes -L {port}:{target} <you>@<this machine>` there, then \
         --hub ws://127.0.0.1:{port})"
    )
}

/// The URL box `name`'s runner dials (`hub`), checked as the runner's own rule has it without
/// `--allow-insecure` (`wss://`, or `ws://` to a loopback host: an ssh tunnel's end), against
/// the listener `config` sets up (`ws://` to a TLS listener is refused), with `ca` (a path on
/// the box) only for `wss://`.
fn dial_in_url(
    config: &Config,
    name: &str,
    hub: Option<&str>,
    ca: Option<&str>,
) -> anyhow::Result<String> {
    let listen = config.hub.listen.as_deref().unwrap_or_default();
    let tls = config.hub.tls.is_some();
    let Some(url) = hub else {
        bail!(
            "give the URL box {name}'s runner dials to reach this hub: --hub <url>. {}",
            reach_hints(listen, tls)
        );
    };
    if let Err(e) = twr_runner::client::check_guided_hub_url(url) {
        bail!(
            "{e:#}\n(--dial-in takes no --allow-insecure. {})",
            reach_hints(listen, tls)
        );
    }
    let plain = url.to_ascii_lowercase().starts_with("ws://");
    if plain && tls {
        bail!("the hub's runner listener serves TLS ([hub.tls]): use wss://, not {url}");
    }
    if let Some(ca) = ca {
        if plain {
            bail!("--ca is for a wss:// hub URL: {url} is plain ws://");
        }
        if ca.chars().any(char::is_control) || !ca.starts_with('/') {
            bail!(
                "--ca {ca:?}: give the CA certificate's absolute path on box {name} (the box's \
                 `twr runner` reads it there)"
            );
        }
    }
    Ok(url.to_string())
}

/// What `twr box add --dial-in` says will happen for box `name` dialing `url`, the hub
/// listening on `listen`, the box going into `file`, the hub running twr `version`.
fn add_lines(name: &str, url: &str, listen: &str, file: &Path, version: &str) -> Vec<String> {
    vec![
        format!("- Its runner connects to the hub at {url} (the hub listens on {listen})."),
        format!(
            "- A new token (32 random bytes) goes into [boxes.{name}] in {} (mode 0600). Hub and \
             runner prove they know it; it never crosses the connection.",
            file.display()
        ),
        format!("- The box needs the hub's twr: {}.", update_hint(version)),
    ]
}

/// How a box gets the hub's twr `version`: a release's `twr update`, or a development build of
/// its commit.
pub(super) fn update_hint(version: &str) -> String {
    match dev_commit(version) {
        Some(commit) => format!(
            "a build of commit {commit} (the hub runs a development build: \
             docs/tools/dev-install.sh in a checkout of that commit, on the box)"
        ),
        None => {
            format!("twr {version} (`twr update --version v{version}` there, or the installer)")
        }
    }
}

/// How box `name`'s runner restarts when it runs as the service `twr runner install-token`
/// printed.
pub(super) fn restart_hint(name: &str) -> String {
    format!(
        "`systemctl --user restart {}`; macOS: `launchctl kickstart -k gui/$(id -u)/{}`",
        unit_name(name),
        agent_label(name)
    )
}

/// What to do on box `name` about a refusal of its runner the hub noted (`reason`), the hub
/// running twr `version`: update twr there for another protocol, install the token again for
/// a proof that does not match. `None` for another reason.
pub(super) fn refusal_fix(name: &str, reason: &str, version: &str) -> Option<String> {
    if reason.contains("protocol") {
        Some(format!(
            "on the box: {}, then restart its runner ({})",
            update_hint(version),
            restart_hint(name)
        ))
    } else if reason.contains("proof") || reason.contains("token") {
        Some(format!(
            "on the box: `twr runner install-token {name}` with the token `twr box token {name}` \
             prints here"
        ))
    } else {
        None
    }
}

/// The command to run on box `name`: `twr runner install-token <name>`, with `--hub` and
/// `--ca` when given, each word shell-quoted.
fn box_command(name: &str, hub: Option<&str>, ca: Option<&str>) -> String {
    let mut cmd = format!("twr runner install-token {}", sh_word(name));
    if let Some(hub) = hub {
        cmd.push_str(&format!(" --hub {}", sh_word(hub)));
    }
    if let Some(ca) = ca {
        cmd.push_str(&format!(" --ca {}", sh_word(ca)));
    }
    cmd
}

/// What `add` and `--rotate` print for box `name`: the box's `command` and its `token`, the
/// token only on a terminal (`terminal`).
fn token_lines(name: &str, command: &str, token: &str, terminal: bool) -> Vec<String> {
    let mut out = vec![format!("On box {name}, run:"), format!("  {command}")];
    if terminal {
        out.extend([
            "and paste this token at its prompt (it is not shown there):".to_string(),
            format!("  {token}"),
            format!("`twr box token {name}` prints it again."),
        ]);
    } else {
        out.push(format!(
            "and paste the box's token at its prompt. The token is not printed (stdout is not a \
             terminal): `twr box token {name}` prints it."
        ));
    }
    out
}

/// Waits for box `name`'s runner to connect (at most [`DIAL_IN_WAIT`], looking every
/// [`ONLINE_POLL`]), printing each new state and each new refusal with what to do about it
/// ([`Watch`]). Online: what `add` prints for an online box. Not an error when it does not
/// connect in time: the box is added, and its runner may start later.
async fn wait(c: &mut Client, config: &Config, name: &str, version: &str) -> anyhow::Result<()> {
    outln!(
        "waiting up to {} minutes for box {name}'s runner to connect (Ctrl-C stops waiting; the \
         box stays added)...",
        DIAL_IN_WAIT.as_secs() / 60
    );
    let start = tokio::time::Instant::now();
    let mut watch = Watch::default();
    loop {
        let info = c.call("hub.info", Value::Null).await?;
        let Some(b) = find_box(&info, name).cloned() else {
            bail!("the hub has no box {name}");
        };
        for l in watch.news(name, &b, version) {
            outln!("{l}");
        }
        if b["state"] == "online" {
            online_note(name, &b);
            project_hints(config, name);
            return Ok(());
        }
        if start.elapsed() >= DIAL_IN_WAIT {
            outln!(
                "box {name}'s runner did not connect within {} minutes; the box stays added. \
                 `twr box status {name}` shows how it goes, `twr box doctor {name}` what the hub \
                 sees",
                DIAL_IN_WAIT.as_secs() / 60
            );
            return Ok(());
        }
        tokio::time::sleep(ONLINE_POLL).await;
    }
}

/// What [`wait`] has said of a box: its state, and the last refusal shown.
#[derive(Debug, Default)]
struct Watch {
    state: String,
    refused: Option<Value>,
}

impl Watch {
    /// The lines box `name`'s `hub.info` entry `b` gives since the last look: a new state, a
    /// new refusal (its reason, and what to do on the box, the hub running twr `version`).
    fn news(&mut self, name: &str, b: &Value, version: &str) -> Vec<String> {
        let mut out = Vec::new();
        let state = str_of(&b["state"]);
        if state != self.state {
            out.push(format!("box {name}: {state}"));
            self.state = state.to_string();
        }
        let refused = Some(b["refused"].clone()).filter(Value::is_object);
        if refused.is_some() && refused != self.refused {
            let reason = str_of(&b["refused"]["reason"]);
            out.push(format!("  {reason}"));
            if let Some(fix) = refusal_fix(name, reason, version) {
                out.push(format!("  {fix}"));
            }
            self.refused = refused;
        }
        out
    }
}

/// What the running hub answered to `box.token` ([`call_box_token`]).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Outcome {
    /// No hub runs.
    NoHub,
    /// A hub that removed its socket was still shutting down after [`crate::HUB_STOP_TIMEOUT`]
    /// ([`hub_or_none`]): it may accept runners until it exits.
    Stopping,
    /// The hub is older than `box.token` (`method_not_found`).
    Older,
    /// The hub has no such box.
    NotFound,
    /// `bad_request`, with its message (the files do not parse; a token too short).
    Refused(String),
    /// `{box, changed, accepting}`.
    Done(Value),
}

/// Has the hub on `socket`, when one runs, read box `name`'s token from the files again
/// (`box.token`). A hub of the workspace (`paths`) that is shutting down is waited for
/// ([`hub_or_none`]).
pub(super) async fn call_box_token(
    paths: &Paths,
    socket: &Path,
    name: &str,
) -> anyhow::Result<Outcome> {
    let mut c = match hub_or_none(paths, socket, crate::HUB_STOP_TIMEOUT).await? {
        Found::Hub(c) => c,
        Found::None => return Ok(Outcome::NoHub),
        Found::Stopping => return Ok(Outcome::Stopping),
    };
    let hello = c.hello(json!({ "client": "cli" })).await?;
    crate::check_workspace(&hello, socket)?;
    match c.call("box.token", json!({ "box": name })).await {
        Ok(v) => Ok(Outcome::Done(v)),
        Err(e) => {
            let rpc = e.downcast_ref::<RpcError>().cloned();
            match rpc {
                Some(r) if r.code == codes::METHOD_NOT_FOUND => Ok(Outcome::Older),
                Some(r) if r.code == codes::NOT_FOUND => Ok(Outcome::NotFound),
                Some(r) if r.code == codes::BAD_REQUEST => Ok(Outcome::Refused(r.message)),
                _ => Err(e),
            }
        }
    }
}

/// What [`hub_or_none`] found.
enum Found {
    Hub(Client),
    None,
    /// A hub that removed its socket had not exited by the deadline.
    Stopping,
}

/// A connection to the hub on `socket`, or none when no hub of the workspace (`paths`) runs.
/// Nothing listening on the socket does not prove it: a hub that is shutting down removes its
/// socket first, and accepts runners, with the token it has, until it exits. Every hub holds
/// the workspace's data-directory lock (`twr.lock`) from before it binds its socket until it
/// exits; a flock, so a hub that crashes lets it go too. While that lock is held and no socket
/// answers, this waits, at most `within`, for the socket to answer or the lock to be free.
/// The lock is only taken to see whether it is free, for one more connection attempt (as `twr
/// hub stop` does), and let go at once: a hub starting meanwhile retries a busy lock for
/// seconds.
async fn hub_or_none(paths: &Paths, socket: &Path, within: Duration) -> anyhow::Result<Found> {
    use crate::StopWorkspaceLock;
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match Client::connect(socket).await {
            Ok(c) => return Ok(Found::Hub(c)),
            Err(e) if no_hub(&e) => {}
            Err(e) => return Err(e),
        }
        match crate::stop_workspace_lock(paths)? {
            StopWorkspaceLock::Missing => return Ok(Found::None),
            StopWorkspaceLock::Free(_lock) => {
                return match Client::connect(socket).await {
                    Ok(c) => Ok(Found::Hub(c)),
                    Err(e) if no_hub(&e) => Ok(Found::None),
                    Err(e) => Err(e),
                }
            }
            StopWorkspaceLock::Held => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(Found::Stopping);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Box `name` as its file has it now: boxes.toml's, else config.toml's (`false`: there).
fn file_box(config_file: &Path, file: &Path, name: &str) -> anyhow::Result<(BoxConfig, bool)> {
    if let Some(b) = BoxesFile::load(file)?.boxes.get(name) {
        return Ok((b.clone(), true));
    }
    match Config::load(config_file)?.boxes.get(name) {
        Some(b) => Ok((b.clone(), false)),
        None => bail!("there is no box {name} in config.toml or boxes.toml"),
    }
}

/// Why box `name` (`cfg`) has no token to show or replace, when it is no token box.
fn no_token_box(name: &str, cfg: &BoxConfig) -> Option<String> {
    match cfg.transport() {
        BoxTransport::Ssh => Some(format!(
            "box {name} is an SSH box: it has no token (the hub connects to it over ssh)"
        )),
        BoxTransport::Command => Some(format!(
            "box {name} is a command box: it has no token (the hub reaches it through its command)"
        )),
        BoxTransport::Token => None,
    }
}

/// `twr box token <name>`: prints the box's token from its file, and nothing else on stdout.
pub(super) fn show(paths: &Paths, name: &str) -> anyhow::Result<()> {
    refuse_agent_action("read box tokens")?;
    default_workspace_only(paths, "token")?;
    twr_core::runner::validate_box_name(name)?;
    let (cfg, in_file) = file_box(&paths.config_file, &paths.boxes_file(), name)?;
    if let Some(why) = no_token_box(name, &cfg) {
        bail!("{why}");
    }
    let Some(token) = cfg.token.as_deref().filter(|t| !t.is_empty()) else {
        if in_file {
            bail!("box {name} has no token: `twr box token {name} --rotate` makes one");
        }
        bail!(
            "box {name} has no token in {}: put one there (`twr runner token` makes one) and \
             restart the hub",
            paths.config_file.display()
        );
    };
    outln!("{token}");
    if std::io::stdout().is_terminal() {
        errln!("on box {name}: `twr runner install-token {name}`, and paste it at its prompt");
    }
    Ok(())
}

/// `twr box token <name> --rotate`: a new token for a box of boxes.toml, written only while
/// the box is still the one read ([`write_rotated`]), then taken by the running hub at once.
pub(super) async fn rotate(
    paths: &Paths,
    socket: &Path,
    name: &str,
    yes: bool,
) -> anyhow::Result<()> {
    refuse_agent_action("change box tokens")?;
    default_workspace_only(paths, "token")?;
    twr_core::runner::validate_box_name(name)?;
    let file = paths.boxes_file();
    let Some(snapshot) = BoxesFile::load(&file)?.boxes.get(name).cloned() else {
        if Config::load(&paths.config_file)?.boxes.contains_key(name) {
            bail!(
                "box {name} is configured in {}, which twr does not edit: put a new token there \
                 (`twr runner token` makes one), then `twr box token {name} --reload`",
                paths.config_file.display()
            );
        }
        bail!("there is no box {name} in config.toml or boxes.toml");
    };
    if let Some(why) = no_token_box(name, &snapshot) {
        bail!("{why}");
    }
    let mut ask = ask_on_stderr;
    let consent = consent(yes, &mut ask);
    if matches!(consent, Consent::NoTerminal) {
        bail!(
            "not rotating box {name}'s token without confirmation (no terminal to ask on): pass \
             --yes"
        );
    }
    outln!(
        "Rotating box {name}'s token: the hub stops accepting the old one now and drops the \
         box's connection. The box stays offline until you install the new token there (`twr \
         runner install-token {name}`); its agents keep running meanwhile."
    );
    if !confirm(
        consent,
        "Rotate? [y/N] ",
        &format!("not rotating box {name}'s token without confirmation"),
    )? {
        outln!("box {name}'s token not changed");
        return Ok(());
    }
    let token = twr_core::runner::new_token()?;
    write_rotated(&paths.config_file, &file, name, &snapshot, &token)?;
    outln!("wrote a new token for box {name} to {}", file.display());
    let outcome = call_box_token(paths, socket, name)
        .await
        .unwrap_or_else(|e| Outcome::Refused(format!("{e:#}")));
    for l in rotated_lines(name, &outcome) {
        outln!("{l}");
    }
    let terminal = std::io::stdout().is_terminal();
    for l in token_lines(name, &box_command(name, None, None), &token, terminal) {
        outln!("{l}");
    }
    outln!("A runner of box {name} that is running takes it at its next connection.");
    if let Some(why) = not_taken(
        name,
        &outcome,
        &format!("box {name}'s new token is in {}", file.display()),
        "the old token",
    ) {
        bail!("{why}");
    }
    Ok(())
}

/// Why the running hub did not take what a rotation or a removal of box `name` changed in
/// boxes.toml (`done`: what the command did), from its answer `outcome`, and how to have it
/// taken: the command then fails, its edit kept, since the hub may still accept `old` (`the
/// old token`, `box gpu's runner`). `None` when the hub took it, or no hub needs to.
pub(super) fn not_taken(name: &str, outcome: &Outcome, done: &str, old: &str) -> Option<String> {
    let restart = "restart the hub (`twr hub stop`, then `twr`)";
    match outcome {
        Outcome::Refused(_) => Some(format!(
            "{done}, but the running hub did not take the change and may still accept {old}: \
             run `twr box token {name} --reload`, or {restart}"
        )),
        Outcome::Older => Some(format!(
            "{done}, but the running hub is older and accepts {old} until it restarts: {restart}"
        )),
        Outcome::Stopping => Some(format!(
            "{done}, but {}",
            stopping(&format!("may accept {old} until it exits"))
        )),
        Outcome::NoHub | Outcome::NotFound | Outcome::Done(_) => None,
    }
}

/// What is said of a hub still shutting down ([`Outcome::Stopping`]): it `does` something
/// until it exits.
fn stopping(does: &str) -> String {
    format!(
        "a hub of this workspace was still shutting down after {}s (its socket is gone, it has \
         not exited) and {does}: the next hub reads the files; check that it exits (see the hub \
         log)",
        crate::HUB_STOP_TIMEOUT.as_secs()
    )
}

/// Writes `token` as box `name`'s in boxes.toml (`file`, next to `config_file`), under the
/// file's lock: only while the box there is the one `snapshot` read, its token included
/// ([`BoxConfig::connection`]; its build and projects aside), so a token changed meanwhile
/// (another rotation, an edit) or a box removed and added again is refused, the file left as
/// it is.
fn write_rotated(
    config_file: &Path,
    file: &Path,
    name: &str,
    snapshot: &BoxConfig,
    token: &str,
) -> anyhow::Result<()> {
    let _lock = BoxesFile::lock(file)?;
    let mut boxes = BoxesFile::load(file)?;
    let Some(b) = boxes.boxes.get_mut(name) else {
        bail!(
            "box {name} was removed from {} meanwhile: nothing was changed",
            file.display()
        );
    };
    if b.connection() != snapshot.connection() {
        bail!(
            "box {name} changed in {} since this command read it: nothing was changed; run `twr \
             box token {name} --rotate` again",
            file.display()
        );
    }
    b.token = Some(token.to_string());
    check_merged(&Config::load(config_file)?, &boxes, file)?;
    boxes.save(file)
}

/// What `--rotate` says of the hub's answer `outcome`.
fn rotated_lines(name: &str, outcome: &Outcome) -> Vec<String> {
    match outcome {
        Outcome::Done(v) if v["accepting"] == true => vec![if v["changed"] == true {
            format!(
                "The running hub took the new token: it no longer accepts the old one, and \
                 dropped box {name}'s connection."
            )
        } else {
            "The running hub has the new token.".to_string()
        }],
        Outcome::Done(_) => vec![format!(
            "The running hub accepts no runner of box {name} now: the files give it no token it \
             takes (`twr box doctor {name}`)"
        )],
        Outcome::NoHub => vec!["No hub runs: one started later reads the new token.".into()],
        Outcome::Stopping => vec!["A hub is still shutting down: it may accept the old token \
                                    until it exits."
            .to_string()],
        Outcome::Older => vec!["The running hub is older: it takes the new token when it \
                                 restarts (`twr hub stop`, then `twr`); until then it accepts the \
                                 old one."
            .to_string()],
        Outcome::NotFound => vec![format!(
            "The running hub has no box {name}: it takes the box, with its new token, when it \
             restarts."
        )],
        Outcome::Refused(why) => vec![format!("The running hub did not take it: {why}")],
    }
}

/// `twr box token <name> --reload`: has the running hub take box `name`'s token from the files
/// again (an edit of config.toml, say), or none when they give it none.
pub(super) async fn reload(paths: &Paths, socket: &Path, name: &str) -> anyhow::Result<()> {
    refuse_agent_action("change box tokens")?;
    default_workspace_only(paths, "token")?;
    twr_core::runner::validate_box_name(name)?;
    match call_box_token(paths, socket, name).await? {
        Outcome::NoHub => {
            outln!("no hub runs: one started later reads box {name}'s token from the files")
        }
        Outcome::Stopping => bail!("{}", stopping("reads no token again")),
        Outcome::Older => bail!(
            "the running hub is older: it reads box tokens only as it starts; restart it (`twr \
             hub stop`, then `twr`)"
        ),
        Outcome::NotFound => bail!(
            "the running hub has no box {name}: restart it (`twr hub stop`, then `twr`), or add \
             the box with `twr box add`"
        ),
        Outcome::Refused(why) => bail!("{why}"),
        Outcome::Done(v) => outln!("{}", reloaded(name, &v)),
    }
    Ok(())
}

/// What `--reload` says of `box.token`'s answer `v`.
fn reloaded(name: &str, v: &Value) -> String {
    match (v["accepting"] == true, v["changed"] == true) {
        (true, true) => format!(
            "the running hub took box {name}'s token from the files: a new one, so it dropped \
             the box's connection (its runner connects again with that token)"
        ),
        (true, false) => {
            format!(
                "the running hub has box {name}'s token from the files already: nothing changed"
            )
        }
        (false, true) => format!(
            "the files give box {name} no token: the running hub no longer accepts its runner, \
             and dropped its connection"
        ),
        (false, false) => {
            format!("the files give box {name} no token: the running hub accepts no runner of it")
        }
    }
}

/// Whether box `name`, just taken out of boxes.toml, is a token box, for what `twr box remove`
/// says next: what the running hub answered to `box.token` (`outcome`: it took the files'
/// token, so its box is one), else the transport of the box it runs (`live`, from `hub.info`),
/// else, with no hub to ask or none that has the box, the removed table's `transport` key
/// (`None` when it has none: a token box; `Some(None)` when it is no string; else the string;
/// also when twr can't load the rest of the table).
pub(super) fn removed_token_box(
    outcome: &Outcome,
    live: Option<&str>,
    transport: Option<Option<&str>>,
) -> bool {
    if let Outcome::Done(_) = outcome {
        return true;
    }
    match (live, transport) {
        (Some(live), _) => live == "token",
        (None, None) => true,
        (None, Some(t)) => t == Some("token"),
    }
}

/// What `twr box remove` says once token box `name` is out of boxes.toml and the running hub
/// answered `outcome` to `box.token`: it no longer accepts the box's runner (then how to stop
/// that runner on the box), or it still does, the box being in `config_file` too.
pub(super) fn removal_lines(name: &str, outcome: &Outcome, config_file: &Path) -> Vec<String> {
    let first = match outcome {
        Outcome::Done(v) if v["accepting"] == true => {
            return vec![format!(
                "box {name} is still configured in {} with a token, and the hub keeps accepting \
                 its runner: remove [boxes.{name}] there and run `twr box token {name} \
                 --reload`, or restart the hub (`twr hub stop`, then `twr`)",
                config_file.display()
            )]
        }
        Outcome::Done(_) => format!("The running hub no longer accepts box {name}'s runner."),
        Outcome::NoHub => "No hub runs: none accepts the box's runner.".to_string(),
        Outcome::Stopping => {
            format!(
                "A hub is still shutting down: it may accept box {name}'s runner until it exits."
            )
        }
        Outcome::NotFound => {
            format!("The running hub has no box {name}: it accepts no runner of it.")
        }
        Outcome::Older => format!(
            "The running hub is older: it accepts box {name}'s runner until it restarts (`twr \
             hub stop`, then `twr`)."
        ),
        Outcome::Refused(why) => format!("The running hub answered: {why}."),
    };
    vec![
        first,
        "On the box, stop its runner and delete its token (as `twr runner install-token` set \
         them up):"
            .to_string(),
        format!("  systemctl --user disable --now {}", unit_name(name)),
        format!(
            "  on macOS instead: launchctl bootout gui/$(id -u)/{label}; rm \
             ~/Library/LaunchAgents/{label}.plist",
            label = agent_label(name)
        ),
        format!("  rm ~/.config/twr/runner-tokens/{name}.token"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config(toml: &str) -> Config {
        Config::from_toml_str(toml).unwrap()
    }

    /// Without `[hub] listen`, the refusal says how to turn it on and reach it; `--hub` is
    /// required and checked with the runner's rule (no plain ws:// off loopback, no
    /// --allow-insecure), refused as ws:// to a TLS listener, and `--ca` only for wss://, as an
    /// absolute path.
    #[test]
    fn the_dial_in_url_fits_the_listener() {
        let off = listener_off(Path::new("/h/.config/twr/config.toml"));
        assert!(
            off.starts_with(
                "a dial-in box connects to the hub's runner listener, which is off. Add to \
                 /h/.config/twr/config.toml:\n  [hub]\n  listen = \"127.0.0.1:7878\"\n"
            ),
            "{off}"
        );
        assert!(
            off.contains("tailscale serve --bg --tls-terminated-tcp=7878 tcp://127.0.0.1:7878")
                && off.contains("--hub ws://127.0.0.1:7878")
                && off.contains("docs/runners.md#dial-in-boxes"),
            "{off}"
        );
        let plain = config("[hub]\nlisten = \"127.0.0.1:9000\"\n");
        let e = dial_in_url(&plain, "gpu", None, None)
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("give the URL box gpu's runner dials to reach this hub: --hub <url>.")
                && e.contains("--tls-terminated-tcp=9000 tcp://127.0.0.1:9000")
                && e.contains("-L 9000:127.0.0.1:9000")
                && e.contains("--hub ws://127.0.0.1:9000"),
            "{e}"
        );
        assert_eq!(
            dial_in_url(&plain, "gpu", Some("wss://hub.ts.net:9000"), None).unwrap(),
            "wss://hub.ts.net:9000"
        );
        assert!(dial_in_url(&plain, "gpu", Some("ws://127.0.0.1:9000"), None).is_ok());
        let e = format!(
            "{:#}",
            dial_in_url(&plain, "gpu", Some("ws://100.64.1.2:9000"), None).unwrap_err()
        );
        assert!(
            e.contains("refusing plain ws:// to 100.64.1.2") && e.contains("no --allow-insecure"),
            "{e}"
        );
        for bad in ["http://h:1", "wss://h :1", "wss://h:1\n"] {
            assert!(
                dial_in_url(&plain, "gpu", Some(bad), None).is_err(),
                "{bad:?}"
            );
        }
        let tls = config(
            "[hub]\nlisten = \"0.0.0.0:7878\"\n[hub.tls]\ncert = \"/c.pem\"\nkey = \"/k.pem\"\n",
        );
        let e = dial_in_url(&tls, "gpu", Some("ws://127.0.0.1:7878"), None).unwrap_err();
        assert!(e.to_string().contains("serves TLS"), "{e}");
        let e = dial_in_url(&tls, "gpu", None, None).unwrap_err();
        assert!(
            e.to_string()
                .contains("--hub wss://<a name in the hub's certificate>:7878"),
            "{e}"
        );
        assert!(dial_in_url(&tls, "gpu", Some("wss://hub:7878"), Some("/etc/ca.pem")).is_ok());
        assert!(dial_in_url(&tls, "gpu", Some("wss://hub:7878"), Some("ca.pem")).is_err());
        assert!(dial_in_url(&plain, "gpu", Some("ws://[::1]:9000"), Some("/ca.pem")).is_err());
        // An unspecified address is tunnelled to on loopback.
        assert!(reach_hints("0.0.0.0:7878", false).contains("-L 7878:127.0.0.1:7878"));
    }

    /// What `add` says before it asks, for a release hub and a development one, and the box
    /// command with its words quoted.
    #[test]
    fn add_says_what_happens_and_the_box_command() {
        let lines = add_lines(
            "gpu",
            "wss://hub:7878",
            "127.0.0.1:7878",
            Path::new("/h/boxes.toml"),
            "0.2.0",
        );
        assert_eq!(
            lines,
            [
                "- Its runner connects to the hub at wss://hub:7878 (the hub listens on \
                 127.0.0.1:7878).",
                "- A new token (32 random bytes) goes into [boxes.gpu] in /h/boxes.toml (mode \
                 0600). Hub and runner prove they know it; it never crosses the connection.",
                "- The box needs the hub's twr: twr 0.2.0 (`twr update --version v0.2.0` there, \
                 or the installer).",
            ]
        );
        let dev = add_lines("gpu", "u", "l", Path::new("/f"), "0.2.0+dev.abc1234");
        assert!(
            dev[2].contains("a build of commit abc1234") && dev[2].contains("dev-install.sh"),
            "{}",
            dev[2]
        );
        assert_eq!(
            box_command("gpu", Some("wss://h:1/a b"), Some("/c a.pem")),
            "twr runner install-token gpu --hub 'wss://h:1/a b' --ca '/c a.pem'"
        );
        assert_eq!(
            box_command("gpu", None, None),
            "twr runner install-token gpu"
        );
    }

    /// The token is printed only on a terminal; otherwise where to get it.
    #[test]
    fn the_token_is_printed_only_on_a_terminal() {
        let token = "ab".repeat(32);
        let on = token_lines("gpu", "twr runner install-token gpu", &token, true).join("\n");
        assert!(on.contains(&format!("\n  {token}\n")), "{on}");
        assert!(on.ends_with("`twr box token gpu` prints it again."), "{on}");
        let off = token_lines("gpu", "twr runner install-token gpu", &token, false).join("\n");
        assert!(!off.contains(&token), "{off}");
        assert!(
            off.contains("stdout is not a terminal): `twr box token gpu` prints it"),
            "{off}"
        );
        assert!(off.starts_with("On box gpu, run:\n  twr runner install-token gpu\n"));
    }

    /// The wait says each new state and each new refusal once, with what to do on the box.
    #[test]
    fn the_wait_says_states_and_refusals_once() {
        let b = |state: &str, refused: Value| json!({"state": state, "refused": refused});
        let mut w = Watch::default();
        assert_eq!(
            w.news("gpu", &b("offline", Value::Null), "0.2.0"),
            ["box gpu: offline"]
        );
        assert!(w
            .news("gpu", &b("offline", Value::Null), "0.2.0")
            .is_empty());
        let proof = json!({"at": "t1", "reason": "a runner was refused: its proof does not match this box's token"});
        let said = w.news("gpu", &b("offline", proof.clone()), "0.2.0");
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(
            said[1].contains("`twr runner install-token gpu` with the token `twr box token gpu`")
        );
        assert!(w.news("gpu", &b("offline", proof), "0.2.0").is_empty());
        let protocol = json!({"at": "t2", "reason": "a runner speaking protocol 6 was refused (the hub's is 7): update twr on box `gpu`"});
        let said = w.news("gpu", &b("offline", protocol), "0.2.0");
        assert!(
            said[1].contains("`twr update --version v0.2.0`")
                && said[1].contains("systemctl --user restart twr-runner-gpu.service")
                && said[1].contains("launchctl kickstart -k gui/$(id -u)/tools.twr.runner.gpu"),
            "{said:?}"
        );
        assert_eq!(
            w.news("gpu", &b("online", Value::Null), "0.2.0"),
            ["box gpu: online"]
        );
        assert_eq!(refusal_fix("gpu", "something else", "0.2.0"), None);
    }

    /// Rotation writes only while the box is the one read, its token included (R3): a token
    /// changed meanwhile, or the box gone, is refused and the file left as it is; a project or
    /// build recorded meanwhile is not.
    #[test]
    fn rotation_writes_only_over_the_token_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("config.toml");
        let file = BoxesFile::path_for(&config_file);
        std::fs::write(&config_file, "").unwrap();
        let save = |c: &BoxConfig| {
            let mut b = BoxesFile::default();
            b.boxes.insert("gpu".into(), c.clone());
            b.save(&file).unwrap();
        };
        let read = token_box(&"a".repeat(64));
        save(&read);
        let mut with_project = read.clone();
        with_project.projects.insert(
            "api".into(),
            twr_core::config::BoxProjectConfig {
                repo: "/h/api".into(),
                runtime: None,
            },
        );
        save(&with_project);
        write_rotated(&config_file, &file, "gpu", &read, &"b".repeat(64)).unwrap();
        let now = BoxesFile::load(&file).unwrap().boxes["gpu"].clone();
        assert_eq!(now.token, Some("b".repeat(64)));
        assert_eq!(now.projects, with_project.projects);
        // Read before that write: its token is no longer the file's.
        let e = write_rotated(&config_file, &file, "gpu", &read, &"c".repeat(64)).unwrap_err();
        assert!(format!("{e:#}").contains("changed in"), "{e:#}");
        assert!(
            format!("{e:#}").contains("twr box token gpu --rotate"),
            "{e:#}"
        );
        assert_eq!(BoxesFile::load(&file).unwrap().boxes["gpu"], now);
        BoxesFile::default().save(&file).unwrap();
        let e = write_rotated(&config_file, &file, "gpu", &now, &"c".repeat(64)).unwrap_err();
        assert!(format!("{e:#}").contains("was removed"), "{e:#}");
    }

    /// `twr box token` reads the box's file, refuses boxes without a token, and says how to
    /// make one per file.
    #[test]
    fn the_token_comes_from_the_box_s_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("config.toml");
        let file = BoxesFile::path_for(&config_file);
        std::fs::write(
            &config_file,
            "[boxes.hand]\ntoken = \"0123456789abcdef0123\"\n[boxes.ssh]\ntransport = \
             \"ssh\"\nssh = \"me@h\"\n",
        )
        .unwrap();
        let mut b = BoxesFile::default();
        b.boxes.insert("gpu".into(), token_box("tok"));
        b.save(&file).unwrap();
        let (gpu, in_file) = file_box(&config_file, &file, "gpu").unwrap();
        assert!(in_file);
        assert_eq!(gpu.token.as_deref(), Some("tok"));
        let (hand, in_file) = file_box(&config_file, &file, "hand").unwrap();
        assert!(!in_file);
        assert_eq!(hand.token.as_deref(), Some("0123456789abcdef0123"));
        let (ssh, _) = file_box(&config_file, &file, "ssh").unwrap();
        assert!(no_token_box("ssh", &ssh)
            .unwrap()
            .contains("SSH box: it has no token"));
        assert!(file_box(&config_file, &file, "none").is_err());
    }

    /// A removal or rotation the running hub did not take (it refused, or is older) fails
    /// with how to have it taken; one it took, or that no hub needs, does not.
    #[test]
    fn a_change_the_hub_did_not_take_fails() {
        let done = Outcome::Done(json!({"box": "gpu", "changed": true, "accepting": false}));
        for o in [Outcome::NoHub, Outcome::NotFound, done] {
            assert_eq!(not_taken("gpu", &o, "did", "the old token"), None, "{o:?}");
        }
        let refused = not_taken(
            "gpu",
            &Outcome::Refused("bad".into()),
            "box gpu is out of /h/boxes.toml",
            "its runner",
        )
        .unwrap();
        assert_eq!(
            refused,
            "box gpu is out of /h/boxes.toml, but the running hub did not take the change and \
             may still accept its runner: run `twr box token gpu --reload`, or restart the hub \
             (`twr hub stop`, then `twr`)"
        );
        let older = not_taken("gpu", &Outcome::Older, "did", "the old token").unwrap();
        assert_eq!(
            older,
            "did, but the running hub is older and accepts the old token until it restarts: \
             restart the hub (`twr hub stop`, then `twr`)"
        );
        let stopping = not_taken("gpu", &Outcome::Stopping, "did", "the old token").unwrap();
        assert_eq!(
            stopping,
            "did, but a hub of this workspace was still shutting down after 30s (its socket is \
             gone, it has not exited) and may accept the old token until it exits: the next hub \
             reads the files; check that it exits (see the hub log)"
        );
    }

    /// A hub that removed its socket but still holds the data directory's lock is waited for:
    /// until the lock is free (no hub), until a socket answers (a hub), or at most the
    /// deadline (still stopping). No data directory: no hub, at once.
    #[tokio::test]
    async fn a_hub_without_its_socket_is_waited_for_while_it_holds_the_data_directory() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(home.path());
        let socket = home.path().join("hub.sock");
        let within = Duration::from_secs(30);
        let found = hub_or_none(&paths, &socket, within).await.unwrap();
        assert!(matches!(found, Found::None), "no data directory");

        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let hold = || {
            let f = std::fs::File::create(paths.data_dir.join(twr_hub::LOCK_FILE)).unwrap();
            f.try_lock().unwrap();
            f
        };
        let lock = hold();
        let found = hub_or_none(&paths, &socket, Duration::from_millis(300))
            .await
            .unwrap();
        assert!(matches!(found, Found::Stopping), "held past the deadline");

        // The lock let go meanwhile (the hub exited, or crashed): no hub.
        let waiting = tokio::spawn({
            let (paths, socket) = (paths.clone(), socket.clone());
            async move { hub_or_none(&paths, &socket, within).await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!waiting.is_finished(), "waits while the lock is held");
        drop(lock);
        let found = tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("done once the lock is free")
            .unwrap()
            .unwrap();
        assert!(matches!(found, Found::None));

        // A socket answers meanwhile (a new hub): that hub.
        let lock = hold();
        let waiting = tokio::spawn({
            let (paths, socket) = (paths.clone(), socket.clone());
            async move { hub_or_none(&paths, &socket, within).await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!waiting.is_finished(), "waits while the lock is held");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let found = tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("done once the socket answers")
            .unwrap()
            .unwrap();
        assert!(matches!(found, Found::Hub(_)));
        drop(lock);
    }

    /// What `remove` says of a token box: revoked (with how to stop the runner on the box), or
    /// still accepted from config.toml (R5), or what the hub could not do. Whether it is one is
    /// the running hub's to say.
    #[test]
    fn removal_follows_the_hubs_answer() {
        let config_file = PathBuf::from("/h/config.toml");
        let done = |accepting: bool| {
            Outcome::Done(json!({"box": "gpu", "changed": true, "accepting": accepting}))
        };
        let revoked = removal_lines("gpu", &done(false), &config_file);
        assert_eq!(
            revoked[0],
            "The running hub no longer accepts box gpu's runner."
        );
        assert_eq!(
            revoked[2],
            "  systemctl --user disable --now twr-runner-gpu.service"
        );
        // The LaunchAgent's plist goes too: launchd would load it again at the next login.
        assert_eq!(
            revoked[3],
            "  on macOS instead: launchctl bootout gui/$(id -u)/tools.twr.runner.gpu; rm \
             ~/Library/LaunchAgents/tools.twr.runner.gpu.plist"
        );
        assert_eq!(revoked[4], "  rm ~/.config/twr/runner-tokens/gpu.token");
        let kept = removal_lines("gpu", &done(true), &config_file);
        assert_eq!(
            kept.len(),
            1,
            "no cleanup while the hub accepts it: {kept:?}"
        );
        assert!(
            kept[0].contains("still configured in /h/config.toml with a token")
                && kept[0].contains("`twr box token gpu --reload`"),
            "{kept:?}"
        );
        for (o, said) in [
            (Outcome::NoHub, "No hub runs"),
            (
                Outcome::Stopping,
                "may accept box gpu's runner until it exits",
            ),
            (Outcome::Older, "until it restarts"),
            (Outcome::NotFound, "has no box gpu"),
            (Outcome::Refused("bad".into()), "answered: bad"),
        ] {
            let l = removal_lines("gpu", &o, &config_file);
            assert!(l[0].contains(said) && l.len() == 5, "{l:?}");
        }
        // The hub decides: it took the files' token, or it runs the box as a token box,
        // whatever the removed table said (edited, misspelled, no string).
        let refused = Outcome::Refused("bad".into());
        for transport in [Some(Some("ssh")), Some(Some("tokn")), Some(None), None] {
            assert!(removed_token_box(&done(false), None, transport));
            assert!(removed_token_box(&refused, Some("token"), transport));
            assert!(removed_token_box(&Outcome::Older, Some("token"), transport));
        }
        // It runs the box as an SSH or a command box.
        assert!(!removed_token_box(&refused, Some("ssh"), None));
        assert!(!removed_token_box(
            &refused,
            Some("command"),
            Some(Some("token"))
        ));
        // No hub, or one without the box: the removed table says.
        for o in [Outcome::NoHub, Outcome::NotFound] {
            assert!(removed_token_box(&o, None, None));
            assert!(removed_token_box(&o, None, Some(Some("token"))));
            assert!(!removed_token_box(&o, None, Some(None)));
            assert!(!removed_token_box(&o, None, Some(Some("ssh"))));
            assert!(!removed_token_box(&o, None, Some(Some("command"))));
        }
    }

    /// `--rotate` and `--reload` say what the hub did.
    #[test]
    fn rotation_and_reload_say_what_the_hub_did() {
        let v = |accepting: bool, changed: bool| json!({"box": "gpu", "accepting": accepting, "changed": changed});
        assert!(rotated_lines("gpu", &Outcome::Done(v(true, true)))[0]
            .contains("dropped box gpu's connection"));
        assert!(rotated_lines("gpu", &Outcome::Older)[0].contains("when it restarts"));
        assert!(rotated_lines("gpu", &Outcome::NoHub)[0].contains("No hub runs"));
        assert!(rotated_lines("gpu", &Outcome::Stopping)[0].contains("still shutting down"));
        assert!(reloaded("gpu", &v(true, true)).contains("dropped the box's connection"));
        assert!(reloaded("gpu", &v(true, false)).contains("nothing changed"));
        assert!(reloaded("gpu", &v(false, true)).contains("no longer accepts its runner"));
        assert!(reloaded("gpu", &v(false, false)).contains("accepts no runner of it"));
    }
}
