//! The hub's boxes: the registry every lookup of a box goes through ([`Boxes`]), the one
//! admission check of a box ([`admit_box`]), and adding a box while the hub runs (`box.add`,
//! `box.install`).
//!
//! **Registry.** The hub's own box and every `[boxes.<name>]` live in one [`Boxes`] behind a
//! read-write lock in `Inner`, reached only through accessors that clone a handle out
//! (`Inner::host`, `Inner::remote`, `Inner::box_config`, ...): no guard is ever held across an
//! `.await` or while the core lock is taken (a lookup under the core lock takes the read lock
//! and lets go at once). The write lock is taken only to add a box ([`register_box`]), never
//! with the core lock held: the name is checked and the box inserted under that one write
//! lock, so of two adds of one name exactly one wins and the other changes nothing. A box,
//! once added, stays until the hub restarts: removing or changing one needs a restart, but for
//! a token box's token (`box.token`, below).
//!
//! **Startup and live add** take the same path: [`admit_box`] (a valid name, not `local`, the
//! transport rules of [`Config::validate`], a token of at least
//! [`twr_core::runner::TOKEN_MIN_LEN`] characters for a token box, a warning below
//! [`twr_core::runner::TOKEN_GOOD_LEN`]), then [`register_box`] (the [`RemoteBox`] built and
//! inserted), then [`start_box`] (the settling of its orphaned commands, and for an SSH or a
//! command box in the default workspace its supervisor, [`crate::ssh_box`]). At startup every
//! box is admitted before the hub opens anything (a box that is refused refuses the hub: a box
//! the hub would skip could never connect anyway), registered before the log's runtimes are
//! looked at (a remote box's are left to its runner), and started once the hub is open.
//!
//! `box.add {name}` (users only) re-reads the hub's config file and the `boxes.toml` next to
//! it ([`Config::load_with_boxes`]) and adds box `name` from them; `box.install {name}` wakes
//! the supervisor of a box the hub connects to (ssh or command) at once (a build was just
//! installed on it); `box.projects {box}` takes a box's `[boxes.<name>.projects]` from them
//! again (`twr box project` just recorded one), the only change to a box the hub takes
//! without a restart besides its build;
//! `box.restart {box}` has such a box's supervisor restart its daemon ([`crate::ssh_box`],
//! "Restarting the daemon"); `box.token {box}` has a token box take its token from them again,
//! or none when they no longer give it one (`twr box token --rotate`, `twr box remove`), and
//! drops its runner connection when that changed it: rotation and revocation are live. The
//! four that read the files (`box.add`, `box.install`, `box.projects`, `box.token`) run one
//! at a time, each from its read to its change (`Inner::box_files_lock`): a removal's
//! `box.token` never runs between a `box.add`'s read of the removed box and its registration.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLockReadGuard, RwLockWriteGuard, Weak};

use serde_json::{json, Value};
use twr_core::config::{error_without_values, BoxConfig, BoxTransport, Config};
use twr_core::proto::{codes, RpcError};
use twr_core::runner::Secret;
use twr_core::sealed::HubKey;

use super::lifecycle::data_dir_of;
use super::{rpc, runners, Caller, Hub, Inner, LOCAL_BOX};
use crate::host::ThreadHost;
use crate::remote::RemoteBox;
use crate::ssh_box::{self, BoxLink, SshBox};

/// Every box the hub knows: its own (`local`) and the remote ones (see the module docs).
pub(super) struct Boxes {
    /// Where threads run, by box: the local box and every remote one.
    hosts: HashMap<String, Arc<dyn ThreadHost>>,
    /// The remote boxes (also in `hosts`).
    remotes: HashMap<String, RemoteBox>,
    /// Each remote box's configuration as it was added (its projects, for `thread.new`).
    configs: BTreeMap<String, BoxConfig>,
    /// The boxes the hub dials (SSH and command boxes, in the default workspace only), with
    /// their supervisor's state.
    ssh: HashMap<String, Arc<SshBox>>,
}

impl Boxes {
    /// The registry with the hub's own box only.
    pub(super) fn with_local(local: Arc<dyn ThreadHost>) -> Boxes {
        Boxes {
            hosts: HashMap::from([(LOCAL_BOX.to_string(), local)]),
            remotes: HashMap::new(),
            configs: BTreeMap::new(),
            ssh: HashMap::new(),
        }
    }
}

impl Inner {
    fn boxes(&self) -> RwLockReadGuard<'_, Boxes> {
        self.boxes.read().unwrap_or_else(|e| e.into_inner())
    }

    fn boxes_mut(&self) -> RwLockWriteGuard<'_, Boxes> {
        self.boxes.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The host of box `name` (`local` or a remote one).
    pub(super) fn host(&self, name: &str) -> Option<Arc<dyn ThreadHost>> {
        self.boxes().hosts.get(name).cloned()
    }

    /// Every box's host, by name (unordered).
    pub(super) fn hosts_list(&self) -> Vec<(String, Arc<dyn ThreadHost>)> {
        let boxes = self.boxes();
        boxes
            .hosts
            .iter()
            .map(|(n, h)| (n.clone(), Arc::clone(h)))
            .collect()
    }

    /// Remote box `name`.
    pub(super) fn remote(&self, name: &str) -> Option<RemoteBox> {
        self.boxes().remotes.get(name).cloned()
    }

    /// Whether `name` is a remote box.
    pub(super) fn has_remote(&self, name: &str) -> bool {
        self.boxes().remotes.contains_key(name)
    }

    /// Every remote box (unordered).
    pub(super) fn remotes_list(&self) -> Vec<RemoteBox> {
        self.boxes().remotes.values().cloned().collect()
    }

    /// Remote box `name`'s configuration as it was added.
    pub(super) fn box_config(&self, name: &str) -> Option<BoxConfig> {
        self.boxes().configs.get(name).cloned()
    }

    /// Every remote box's configuration as it was added, by name.
    pub(super) fn box_configs(&self) -> BTreeMap<String, BoxConfig> {
        self.boxes().configs.clone()
    }

    /// SSH box `name`'s supervisor state, when the hub dials it.
    pub(super) fn ssh_box(&self, name: &str) -> Option<Arc<SshBox>> {
        self.boxes().ssh.get(name).cloned()
    }

    /// Every SSH box the hub dials (unordered).
    pub(super) fn ssh_boxes(&self) -> Vec<Arc<SshBox>> {
        self.boxes().ssh.values().cloned().collect()
    }

    /// Adds `warning` to the hub's warnings (`hub.info`), unless it is there already, and logs
    /// it.
    pub(super) fn warn(&self, warning: String) {
        let mut w = self.warnings.lock().unwrap_or_else(|e| e.into_inner());
        if !w.contains(&warning) {
            tracing::warn!("{warning}");
            w.push(warning);
        }
    }

    /// The hub's warnings (`hub.info`).
    pub(super) fn warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The one check a box passes before anything of it is published or started, at startup and
/// on a live add (see the module docs): a valid box name that is not `local`, the transport
/// rules of [`Config::validate`] (an SSH box has an `ssh` destination and no token, a command
/// box a `command` and neither; `build` only on those, and a build id), `ssh` words that
/// forward nothing and leave how ssh runs to twr ([`twr_core::config::check_ssh_words`], a
/// token box's too: `twr attach` runs them), and for a token box a token of at least
/// [`twr_core::runner::TOKEN_MIN_LEN`] characters. Returns the warning for a token shorter
/// than [`twr_core::runner::TOKEN_GOOD_LEN`]. A token box without a token is admitted (it can
/// never connect; logged).
pub(super) fn admit_box(name: &str, cfg: &BoxConfig) -> anyhow::Result<Option<String>> {
    twr_core::runner::validate_box_name(name)?;
    if name == LOCAL_BOX {
        anyhow::bail!("box name {LOCAL_BOX:?} is the hub's own box: name the remote box otherwise");
    }
    let mut alone = Config::default();
    alone.boxes.insert(name.to_string(), cfg.clone());
    alone.validate()?;
    // A token box's `ssh` (`twr attach` only) too: `validate` checks only an SSH box's.
    cfg.check_ssh(name)?;
    match &cfg.token {
        Some(token) if cfg.transport() == BoxTransport::Token => {
            twr_core::runner::check_token_len(token, &format!("boxes.{name}.token"))
                .map_err(anyhow::Error::msg)
        }
        _ => Ok(None),
    }
}

/// Why [`register_box`] did not add a box.
#[derive(Debug)]
pub(super) enum AddBoxError {
    /// [`admit_box`] refused it.
    Refused(anyhow::Error),
    /// The hub has a box of that name already (nothing was replaced).
    Conflict(String),
}

impl From<AddBoxError> for RpcError {
    fn from(e: AddBoxError) -> RpcError {
        match e {
            AddBoxError::Refused(e) => rpc(codes::BAD_REQUEST, format!("{e:#}")),
            AddBoxError::Conflict(name) => rpc(
                codes::CONFLICT,
                format!(
                    "the hub has box {name} already (changing a box needs a hub restart: `twr \
                     hub stop`, then `twr`)"
                ),
            ),
        }
    }
}

/// Admits box `name` ([`admit_box`]; its short-token warning goes to the hub's warnings),
/// builds its [`RemoteBox`] (the hub's park-disposable paths and change timeout; an SSH or a
/// command box also the branch namespace of the hub's own box, see [`crate::remote`]: its
/// daemon belongs to this hub instance) and, for one of those in the default workspace, its
/// supervisor's state ([`SshBox`]), then checks the name and
/// inserts all of it under one write lock. Starts nothing ([`start_box`]).
pub(super) fn register_box(
    inner: &Inner,
    name: &str,
    cfg: BoxConfig,
) -> Result<(RemoteBox, Option<Arc<SshBox>>), AddBoxError> {
    let warning = admit_box(name, &cfg).map_err(AddBoxError::Refused)?;
    let rbox = match cfg.transport() {
        BoxTransport::Token => {
            let token = Secret::new(cfg.token.clone().unwrap_or_default());
            if token.is_empty() {
                tracing::warn!(box_name = %name, "box has no token: its runner cannot connect");
            }
            RemoteBox::new(name, token, cfg.ssh.clone())
        }
        BoxTransport::Ssh => {
            // (`admit_box` checked that it has one.)
            RemoteBox::new_ssh(name, cfg.ssh.clone().unwrap_or_default())
                .with_branch_namespace(inner.worktree_namespace.clone())
        }
        BoxTransport::Command => {
            // (`admit_box` checked that it has one.)
            let Some(link) = BoxLink::of(&cfg) else {
                return Err(AddBoxError::Refused(anyhow::anyhow!(
                    "boxes.{name}: a command box needs `command`"
                )));
            };
            RemoteBox::new_command(name, link)
                .with_branch_namespace(inner.worktree_namespace.clone())
        }
    }
    .with_park_disposable(inner.config.threads.park_disposable.clone())
    .with_change_timeout(inner.change_request_timeout);
    // A named workspace's hub never dials boxes, as it never listens.
    let ssh = (cfg.transport().dialed() && inner.workspace.is_none())
        .then(|| Arc::new(SshBox::new(name, &cfg)));
    {
        let mut boxes = inner.boxes_mut();
        if boxes.hosts.contains_key(name) {
            return Err(AddBoxError::Conflict(name.to_string()));
        }
        boxes.hosts.insert(name.to_string(), Arc::new(rbox.clone()));
        boxes.remotes.insert(name.to_string(), rbox.clone());
        boxes.configs.insert(name.to_string(), cfg);
        if let Some(s) = &ssh {
            boxes.ssh.insert(name.to_string(), Arc::clone(s));
        }
    }
    if let Some(w) = warning {
        inner.warn(w);
    }
    Ok((rbox, ssh))
}

/// Starts what a registered box runs: the settling of its orphaned commands
/// ([`runners::settle_orphans`]) and, for a box the hub dials, its supervisor.
pub(super) fn start_box(inner: &Arc<Inner>, rbox: RemoteBox, ssh: Option<Arc<SshBox>>) {
    tokio::spawn(runners::settle_orphans(
        Arc::downgrade(inner),
        rbox.clone(),
        inner.state.subscribe(),
    ));
    if let Some(s) = ssh {
        ssh_box::start(WeakHub(Arc::downgrade(inner)), rbox, s);
    }
}

/// Adds box `name` while the hub runs: [`register_box`], then [`start_box`].
pub(super) fn add_box(inner: &Arc<Inner>, name: &str, cfg: BoxConfig) -> Result<(), AddBoxError> {
    let (rbox, ssh) = register_box(inner, name, cfg)?;
    start_box(inner, rbox, ssh);
    Ok(())
}

impl Hub {
    /// `box.add {name}` (users only, see the module docs): box `name` from the hub's config
    /// file and the `boxes.toml` next to it, read again now, added to the running hub. Returns
    /// `{box, transport, listening, already}` (`listening`: the hub's runner listener is up,
    /// without which a token box cannot connect; `already`: the hub had the box, with the
    /// settings the files give it now, its build aside: nothing changed). `unauthorized` for
    /// the orchestrator, `unsupported` in a named workspace (boxes belong to the default
    /// workspace's hub) or for a hub that has no config file, `bad_request` when the files do
    /// not parse or the box is refused, `not_found` when neither file has the box, `conflict`
    /// when the hub has a box of that name with other settings. Serialized with `box.token`,
    /// `box.install` and `box.projects` from the read to the registration.
    pub async fn box_add(&self, name: &str, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user adds boxes (`twr box add`)",
            ));
        }
        if let Some(w) = &self.inner.workspace {
            return Err(rpc(
                codes::UNSUPPORTED,
                format!(
                    "boxes belong to the default workspace's hub, not workspace {w}'s: add it \
                     without -w"
                ),
            ));
        }
        let Some(file) = self.inner.config_file.clone() else {
            return Err(rpc(
                codes::UNSUPPORTED,
                "this hub was started without a config file to read boxes from",
            ));
        };
        // One at a time (with `box.token` and the others), from the read to the registration:
        // a removal's `box.token` either finds the box registered, and revokes it, or comes
        // first, and this read no longer finds it.
        let _one = self.inner.box_files_lock.lock().await;
        let config = tokio::task::spawn_blocking(move || {
            Config::load_with_boxes(&file).map_err(|e| error_without_values(&e))
        })
        .await
        .map_err(|e| rpc(codes::INTERNAL, format!("reading the config: {e}")))?
        .map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        if let Some(hook) = &self.inner.box_files_read_hook {
            hook(name).await;
        }
        let Some(cfg) = config.boxes.get(name).cloned() else {
            return Err(rpc(
                codes::NOT_FOUND,
                format!(
                    "no box {name:?} in config.toml or boxes.toml (`twr box add {name} \
                     <ssh destination>` adds one)"
                ),
            ));
        };
        let transport = cfg.transport();
        let already = match add_box(&self.inner, name, cfg.clone()) {
            Ok(()) => {
                tracing::info!(box_name = %name, ?transport, "box added");
                false
            }
            // The hub has it, with the settings the files give it now (it read them as it
            // started, after `twr box add` wrote them, say): nothing to do. Other settings:
            // a conflict (the hub takes a box's new settings when it restarts).
            Err(AddBoxError::Conflict(n))
                if self.inner.box_config(name).as_ref().map(without_build)
                    == Some(without_build(&cfg)) =>
            {
                tracing::debug!(box_name = %n, "box.add of a box the hub has as it is");
                true
            }
            Err(e) => return Err(e.into()),
        };
        Ok(json!({
            "box": name,
            "transport": transport_name(transport),
            "listening": self.inner.listening.load(std::sync::atomic::Ordering::Relaxed),
            "already": already,
        }))
    }

    /// `box.install {name}` (users only): wakes box `name`'s supervisor at once (an SSH or a
    /// command box; the CLI
    /// just installed a build on it), and returns `{box}`. The box's `build` is read again
    /// from the hub's config file and the `boxes.toml` next to it first (`twr box install`
    /// records there the build it installed for another OS or CPU); when they cannot be read,
    /// or no longer have the box, the build the hub knew stays. Only the build is taken: a box
    /// whose other settings changed there since the hub added it (its ssh destination or
    /// command, say, where the CLI just installed, while the hub dials the old one; its
    /// projects aside, which `box.projects` takes) is `conflict`, nothing changed: the hub
    /// takes a box's new settings when it restarts. `unauthorized` for the
    /// orchestrator, `not_found` for a box the hub does not have, `bad_request` for a token
    /// box (or one the hub does not dial: a named workspace's).
    pub async fn box_install(&self, name: &str, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user installs twr on boxes (`twr box install`)",
            ));
        }
        let Some(rbox) = self.inner.remote(name) else {
            return Err(rpc(
                codes::NOT_FOUND,
                format!("the hub has no box {name:?}"),
            ));
        };
        if !rbox.transport().dialed() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "box {name} is not a box the hub connects to (ssh or command): its runner \
                     connects to the hub itself"
                ),
            ));
        }
        let Some(sbox) = self.inner.ssh_box(name) else {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "this hub does not connect to box {name} (only the default workspace's does)"
                ),
            ));
        };
        // One at a time (with `box.add` and the others), from its read of the files to its
        // change.
        let _one = self.inner.box_files_lock.lock().await;
        if let Some(file) = self.inner.config_file.clone() {
            let read = tokio::task::spawn_blocking(move || Config::load_with_boxes(&file)).await;
            match read {
                Ok(Ok(config)) => match config.boxes.get(name) {
                    Some(cfg) if cfg.transport().dialed() => {
                        let registered = self.inner.box_config(name);
                        if registered.as_ref().map(connection) != Some(connection(cfg)) {
                            return Err(rpc(
                                codes::CONFLICT,
                                format!(
                                    "box {name}'s settings changed in the config files since the \
                                     hub added it (its ssh destination or command, say): the \
                                     hub does not take a new build for it until it reads them \
                                     again. Restart the hub (`twr hub stop`, then `twr`)"
                                ),
                            ));
                        }
                        if let Some(c) = self.inner.boxes_mut().configs.get_mut(name) {
                            c.build = cfg.build.clone();
                        }
                        sbox.set_build(cfg.build.clone());
                    }
                    _ => {
                        tracing::warn!(box_name = %name, "box.install: the files no longer have the box (ssh or command); keeping its build")
                    }
                },
                Ok(Err(e)) => {
                    tracing::warn!(box_name = %name, error = %error_without_values(&e), "box.install: the config cannot be read; keeping the box's build")
                }
                Err(e) => {
                    tracing::warn!(box_name = %name, error = %e, "box.install: reading the config failed")
                }
            }
        }
        sbox.wake();
        Ok(json!({ "box": name }))
    }

    /// `box.restart {box}` (users only): box `name`'s daemon (an SSH or a command box's) is
    /// restarted by its
    /// supervisor (see [`crate::ssh_box`], "Restarting the daemon"): the session that is up
    /// ends (the box is offline until the next one; its runtimes end with the daemon), and the
    /// next attempt, at once, has the relay stop whatever daemon answers for this hub's key
    /// and start a new one. Returns at once `{box, epoch}`: the epoch of the connection being
    /// replaced (null when none is up); `hub.info` shows the box online again on a larger
    /// epoch, with `restart_pending` false, once a new daemon serves it. `unauthorized` for
    /// the orchestrator, `not_found` for a box the hub does not have, `unsupported` in a named
    /// workspace (its hub does not connect to boxes), `bad_request` for a token box.
    pub fn box_restart(&self, name: &str, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user restarts box daemons (`twr box restart`)",
            ));
        }
        if let Some(w) = &self.inner.workspace {
            return Err(rpc(
                codes::UNSUPPORTED,
                format!(
                    "boxes belong to the default workspace's hub, not workspace {w}'s: restart \
                     it without -w"
                ),
            ));
        }
        let Some(rbox) = self.inner.remote(name) else {
            return Err(rpc(
                codes::NOT_FOUND,
                format!("the hub has no box {name:?}"),
            ));
        };
        if !rbox.transport().dialed() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "box {name} is not a box the hub connects to (ssh or command): its runner \
                     connects to the hub itself; restart that `twr runner` on the box"
                ),
            ));
        }
        let Some(sbox) = self.inner.ssh_box(name) else {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "this hub does not connect to box {name} (only the default workspace's does)"
                ),
            ));
        };
        let epoch = rbox.epoch();
        tracing::info!(box_name = %name, ?epoch, "restarting the box's daemon");
        sbox.request_restart();
        Ok(json!({ "box": name, "epoch": epoch }))
    }

    /// `box.projects {box}` (users only): box `name`'s projects (`[boxes.<name>.projects]`,
    /// where each repository is on the box) read again from the hub's config file and the
    /// `boxes.toml` next to it (`twr box project` just recorded one there), replacing the ones
    /// the hub has for that box, and nothing else of it or of any other box; a new thread on
    /// the box uses them from now on. Returns `{box, projects, repos}` (`projects` the names,
    /// sorted; `repos` each one's `repo` as the hub has it now, by name). Serialized with
    /// `box.add`, `box.install` and `box.token` from before the read to the change, so an
    /// older read is never applied after a newer one. `unauthorized` for the orchestrator,
    /// `not_found` for a box the hub does not have or the files no longer have, `unsupported`
    /// for a hub without a config file, `bad_request` when the files do not parse, `conflict`
    /// when the box's other settings there (its transport, `ssh`, token) differ from the ones
    /// the hub added it with (nothing changed: restart the hub).
    pub async fn box_projects(&self, name: &str, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user records projects on boxes (`twr box project`)",
            ));
        }
        if !self.inner.has_remote(name) {
            return Err(rpc(
                codes::NOT_FOUND,
                format!("the hub has no box {name:?}"),
            ));
        }
        let Some(file) = self.inner.config_file.clone() else {
            return Err(rpc(
                codes::UNSUPPORTED,
                "this hub was started without a config file to read boxes from",
            ));
        };
        // One at a time (with `box.install` and the others), from its read of the files to its
        // change.
        let _one = self.inner.box_files_lock.lock().await;
        let config = tokio::task::spawn_blocking(move || {
            Config::load_with_boxes(&file).map_err(|e| error_without_values(&e))
        })
        .await
        .map_err(|e| rpc(codes::INTERNAL, format!("reading the config: {e}")))?
        .map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        if let Some(hook) = &self.inner.box_files_read_hook {
            hook(name).await;
        }
        let Some(cfg) = config.boxes.get(name) else {
            return Err(rpc(
                codes::NOT_FOUND,
                format!(
                    "config.toml and boxes.toml no longer have box {name}: the hub keeps it as it \
                     was until it restarts"
                ),
            ));
        };
        let projects: Vec<&String> = cfg.projects.keys().collect();
        // What each one is now, for the caller to compare with what it wrote.
        let repos: serde_json::Map<String, Value> = cfg
            .projects
            .iter()
            .map(|(p, c)| (p.clone(), json!(c.repo.to_string_lossy())))
            .collect();
        {
            let mut boxes = self.inner.boxes_mut();
            let Some(registered) = boxes.configs.get_mut(name) else {
                return Err(rpc(
                    codes::NOT_FOUND,
                    format!("the hub has no box {name:?}"),
                ));
            };
            if connection(registered) != connection(cfg) {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "box {name}'s settings changed in the config files since the hub added \
                         it (its ssh destination, say): the hub does not take its projects until \
                         it reads them again. Restart the hub (`twr hub stop`, then `twr`)"
                    ),
                ));
            }
            registered.projects = cfg.projects.clone();
        }
        tracing::info!(box_name = %name, projects = ?projects, "box projects read again");
        Ok(json!({ "box": name, "projects": projects, "repos": repos }))
    }

    /// `box.token {box}` (users only): token box `name` takes the token the hub's config file
    /// and the `boxes.toml` next to it give it now, read again (`twr box token --rotate` or
    /// `twr box remove` just changed them): the files' token when they have the box as a token
    /// box with a token, else none (revoked: no runner of the box is accepted). The hub's
    /// token is only ever the files' token, or none. A token shorter than
    /// [`twr_core::runner::TOKEN_MIN_LEN`] is not taken: the old one is revoked all the same,
    /// and the answer is `bad_request` with why; a short one is taken with a warning, as at
    /// startup. A change drops the box's runner connection at once (the box goes offline) and
    /// refuses a handshake that proved the old token (`hub::runners`). Nothing else of the box
    /// changes; the configuration the hub compares `box.add`, `box.install` and
    /// `box.projects` against takes the same token. Returns `{box, changed, accepting}`
    /// (`accepting`: the box has a token now), never the token. Serialized with `box.add`,
    /// `box.install` and `box.projects` from before the box is looked up to the change (a
    /// `box.add` that read the old files ends before it, or reads the new ones). `unauthorized`
    /// for the orchestrator, `unsupported` in a named workspace or for a hub without a config
    /// file, `not_found` for a box the hub does not have, `bad_request` for a box the hub
    /// connects to (ssh or command: it has no token) or files that do not parse.
    pub async fn box_token(&self, name: &str, caller: &Caller) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user changes box tokens (`twr box token`, `twr box remove`)",
            ));
        }
        if let Some(w) = &self.inner.workspace {
            return Err(rpc(
                codes::UNSUPPORTED,
                format!(
                    "boxes belong to the default workspace's hub, not workspace {w}'s: run it \
                     without -w"
                ),
            ));
        }
        let Some(file) = self.inner.config_file.clone() else {
            return Err(rpc(
                codes::UNSUPPORTED,
                "this hub was started without a config file to read boxes from",
            ));
        };
        // One at a time (with `box.add` and the others), from before the box is looked up to
        // the change: a `box.add` that read the box's token before a removal has registered
        // it by then, and is revoked here.
        let _one = self.inner.box_files_lock.lock().await;
        let Some(rbox) = self.inner.remote(name) else {
            return Err(rpc(
                codes::NOT_FOUND,
                format!("the hub has no box {name:?}"),
            ));
        };
        if rbox.transport() != BoxTransport::Token {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "box {name} is a box the hub connects to (ssh or command): it has no token"
                ),
            ));
        }
        let config = tokio::task::spawn_blocking(move || {
            Config::load_with_boxes(&file).map_err(|e| error_without_values(&e))
        })
        .await
        .map_err(|e| rpc(codes::INTERNAL, format!("reading the config: {e}")))?
        .map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        let files = config
            .boxes
            .get(name)
            .filter(|c| c.transport() == BoxTransport::Token)
            .and_then(|c| c.token.clone());
        // The files' token, checked as at startup; one too short is not taken.
        let (token, warning, refused) = match files {
            Some(t) => {
                match twr_core::runner::check_token_len(&t, &format!("boxes.{name}.token")) {
                    Ok(w) => (Some(t), w, None),
                    Err(e) => (None, None, Some(e)),
                }
            }
            None => (None, None, None),
        };
        let accepting = token.is_some();
        let changed = runners::set_box_token(
            &self.inner,
            &rbox,
            Secret::new(token.clone().unwrap_or_default()),
        );
        if let Some(c) = self.inner.boxes_mut().configs.get_mut(name) {
            c.token = token;
        }
        if let Some(w) = warning {
            self.inner.warn(w);
        }
        tracing::info!(box_name = %name, changed, accepting, "box token read again");
        if let Some(e) = refused {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("{e}; the hub no longer accepts box {name}'s runner"),
            ));
        }
        Ok(json!({ "box": name, "changed": changed, "accepting": accepting }))
    }

    /// Adds box `name` with `cfg` as `box.add` would, without reading any file (tests).
    #[doc(hidden)]
    pub fn add_box_config(&self, name: &str, cfg: BoxConfig) -> anyhow::Result<()> {
        add_box(&self.inner, name, cfg)
            .map_err(|e| anyhow::anyhow!("{}", RpcError::from(e).message))
    }
}

/// A handle on the hub that does not keep it alive (an SSH box's supervisor holds one between
/// its attempts, [`crate::ssh_box`]).
#[derive(Clone)]
pub(crate) struct WeakHub(Weak<Inner>);

impl WeakHub {
    /// The hub, while it lives.
    pub(crate) fn upgrade(&self) -> Option<Hub> {
        self.0.upgrade().map(|inner| Hub { inner })
    }
}

impl Hub {
    /// The hub's box key ([`twr_core::sealed::HubKey`], `<data dir>/box-key`), loaded or created
    /// on the first call that succeeds and kept.
    pub(crate) async fn box_key(&self) -> anyhow::Result<Arc<HubKey>> {
        let path = data_dir_of(&self.inner.db_path).join(ssh_box::BOX_KEY_FILE);
        self.inner
            .box_key
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || HubKey::load_or_create(&path))
                    .await?
                    .map(Arc::new)
            })
            .await
            .cloned()
    }

    /// [`HubOptions::box_dialer`](crate::HubOptions::box_dialer): the SSH boxes' test dialer.
    pub(crate) fn box_dialer(&self) -> Option<ssh_box::BoxDialer> {
        self.inner.box_dialer.clone()
    }

    /// [`HubOptions::box_switch`](crate::HubOptions::box_switch).
    pub(crate) fn box_switch_timing(&self) -> ssh_box::SwitchTiming {
        self.inner.box_switch
    }

    /// [`Hub::serve_sealed_runner`], telling `welcomed` the inventory's build once the welcome
    /// went out (the ssh supervisor).
    pub(crate) async fn serve_sealed_session<S>(
        &self,
        ws: tokio_tungstenite::WebSocketStream<S>,
        box_name: &str,
        welcomed: tokio::sync::oneshot::Sender<Option<String>>,
    ) -> runners::SealedEnd
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        runners::serve(
            Arc::clone(&self.inner),
            ws,
            format!("ssh:{box_name}"),
            runners::Admission::Sealed(box_name.to_string()),
            Some(welcomed),
        )
        .await
    }

    /// The runner listener serves this hub, or no longer does ([`crate::listener`]).
    pub(crate) fn set_listening(&self, on: bool) {
        self.inner
            .listening
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// `hub.info`'s `boxes`: each remote box with its transport, state and agents (see
    /// SKILL.md), as a user sees it (`box_agents::project_box` makes the orchestrator's view).
    pub(super) fn boxes_json(&self) -> Vec<Value> {
        let remotes = self.inner.remotes_list();
        let ssh: HashMap<String, Arc<SshBox>> = self
            .inner
            .ssh_boxes()
            .into_iter()
            .map(|s| (s.name().to_string(), s))
            .collect();
        let online: Vec<(bool, bool)> = {
            let core = self.inner.core();
            remotes
                .iter()
                .map(|b| {
                    (
                        core.online_boxes.contains(b.name()),
                        core.switching_boxes.contains(b.name()),
                    )
                })
                .collect()
        };
        let mut out: Vec<(String, Value)> = remotes
            .iter()
            .zip(online)
            .map(|(b, (online, switching))| {
                let name = b.name().to_string();
                // Refusals only matter while no runner of the box is connected.
                let refused = (!online)
                    .then(|| b.refusal())
                    .flatten()
                    .map(|r| json!({"at": r.at, "reason": r.reason}));
                let st = ssh.get(&name).map(|s| s.status());
                let state = match (&st, online) {
                    // Being switched to the build launched there (`hub::box_switch`).
                    _ if switching => "switching",
                    (_, true) => "online",
                    (Some(st), false) => st.state.as_str(),
                    (None, false) => "offline",
                };
                let st = st.unwrap_or_default();
                // The fields of a box the hub dials (ssh or command).
                let ssh_box = b.transport().dialed();
                let v = json!({
                    "name": name,
                    "transport": transport_name(b.transport()),
                    "state": state,
                    "online": online,
                    "version": b.info().map(|i| i.version),
                    "refused": refused,
                    "last_error": st.last_error,
                    "notes": ssh_box.then_some(st.notes),
                    "os": st.os,
                    "arch": st.arch,
                    "build": st.build,
                    "running_build": st.running,
                    "switch_pending": ssh_box.then_some(st.switch_pending),
                    "restart_pending": ssh_box.then_some(st.restart_pending),
                    "epoch": b.epoch(),
                    "launch": b.info().and_then(|i| i.launch),
                    "agents": b.agents(),
                });
                (name, v)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.into_iter().map(|(_, v)| v).collect()
    }

    /// Waits (at most `within` each) for the supervisors of the boxes the hub dials to end,
    /// once the hub stopped: each kills its ssh or command as it ends.
    pub(super) async fn ssh_boxes_ended(&self, within: std::time::Duration) {
        for s in self.inner.ssh_boxes() {
            s.ended(within).await;
        }
    }
}

/// `c` but its `build` (which `twr box install` changes while the hub runs).
fn without_build(c: &BoxConfig) -> BoxConfig {
    BoxConfig {
        build: None,
        ..c.clone()
    }
}

/// How the hub reaches box `c` ([`BoxConfig::connection`]).
fn connection(c: &BoxConfig) -> BoxConfig {
    c.connection()
}

/// How `hub.info` and `box.add` name a transport.
pub(super) fn transport_name(t: BoxTransport) -> &'static str {
    match t {
        BoxTransport::Token => "token",
        BoxTransport::Ssh => "ssh",
        BoxTransport::Command => "command",
    }
}
