//! The workspaces of a root ([`crate::paths`]): what each is called, which one commands
//! without `-w` select, and the registry operations that change them (`twr ws new`, `rename`,
//! `default`, and the bookkeeping of `rm`).
//!
//! A workspace has an **id**, its storage slot, which never changes: [`DEFAULT_WORKSPACE`]
//! for the one in the root's own data directory (the *root slot*), else the name of its
//! directory under `<root data>/workspaces/`. Everything a workspace keeps is keyed by its id:
//! its data directory, its socket (`ws/<id>.sock`), its branches (`@<id>`), its orchestrator's
//! working directory (which Claude Code keys its sessions and memories by). A workspace is
//! created with its name as its id.
//!
//! Its **name** is what it is shown as: the [`NAME_FILE`] in its data directory, else its id.
//! Renaming writes that file and moves nothing, so a running hub, its threads, sessions,
//! worktrees and the TUIs on it keep going.
//!
//! A selector (`-w`, `$TWR_WORKSPACE`, `/ws <x>`) is a name or an id. Names and ids share one
//! namespace: no workspace may be named another's name or id, and only the root slot may be
//! named `default`. So a selector names one workspace at most, and a renamed workspace's
//! original name keeps selecting it (until it is removed): an alias of that workspace, not a
//! workspace of its own.
//!
//! The **default** workspace, which commands without `-w` (and `$TWR_WORKSPACE`) select, is
//! the one whose id [`DEFAULT_FILE`] holds, else the root slot. There can be none: the root
//! slot was removed and no other was made the default.
//!
//! The root slot can be removed like any other: [`REMOVED_FILE`] in the root's data directory
//! says so (the directory itself holds what every workspace shares, so it stays), and no hub
//! starts there until `twr ws new default` makes it again.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};

use crate::paths::{workspace_name_error, Paths, DEFAULT_WORKSPACE};

/// The file in a workspace's data directory that holds its name, when it was renamed.
pub const NAME_FILE: &str = "workspace-name";

/// The file in a workspace's data directory that holds a token of its own, made with it: a
/// workspace removed and made again with the same id has another ([`generation`]).
pub const GENERATION_FILE: &str = "workspace-generation";

/// The file in the root's data directory that holds the default workspace's id, once one
/// other than the root slot was made the default (`twr ws default`).
pub const DEFAULT_FILE: &str = "default-workspace";

/// The file in the root's data directory that says the root slot's workspace was removed.
pub const REMOVED_FILE: &str = "workspace-removed";

/// Set by a process that starts a hub for a workspace it knows by id (`twr`, the desktop app,
/// with `--workspace <id>`): the hub serves that id or none, never a workspace that a name
/// fallback would find once that one is gone ([`Paths::find_workspace`]). The hub removes it
/// from what it runs.
pub const HUB_ID_ENV: &str = "TWR_HUB_WORKSPACE_ID";

/// The lock in the root's data directory held while workspaces are created, renamed, removed
/// or made the default, so two of those never pass the same check.
pub const REGISTRY_LOCK: &str = "workspaces.lock";

/// What the root slot's workspace keeps in the root's data directory, and its removal takes
/// (entries starting with one of [`ROOT_OWNED_PREFIXES`] too). Everything else there is
/// shared by every workspace (`workspaces/`, `accounts/`, `job-slots/`, the TUI's and the
/// speaker's state, the boxes' key) or not twr's, and stays.
pub const ROOT_OWNED: &[&str] = &[
    "twr.db",
    "twr.db-wal",
    "twr.db-shm",
    "twr.db-journal",
    "archive.db",
    "archive.db-wal",
    "archive.db-shm",
    "archive.db-journal",
    "hub.log",
    "trash",
    "jobs",
    "suggest",
    "memory",
    "loops",
    "worktrees",
    "tui-history",
    // Its identity to the runner service (`twr-link`): one made again is another workspace.
    "link-identity",
    NAME_FILE,
    GENERATION_FILE,
];

/// Prefixes of the root slot's own files in the root's data directory: its orchestrator's
/// MCP configs (`orch-mcp-<hash>.json`).
pub const ROOT_OWNED_PREFIXES: &[&str] = &["orch-mcp-"];

/// A workspace that exists, as listed ([`Paths::workspaces`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its id ([`crate::workspaces`]).
    pub id: String,
    /// Its name.
    pub name: String,
    /// Whether commands without `-w` select it.
    pub default: bool,
    /// Its paths.
    pub paths: Paths,
}

impl Paths {
    /// This workspace's id: [`DEFAULT_WORKSPACE`] for the root slot, else its directory's name.
    pub fn workspace_id(&self) -> &str {
        self.workspace.as_deref().unwrap_or(DEFAULT_WORKSPACE)
    }

    /// This workspace's name: its [`NAME_FILE`] when that holds a valid name, else its id.
    pub fn display_name(&self) -> String {
        read_name(&self.data_dir.join(NAME_FILE)).unwrap_or_else(|| self.workspace_id().into())
    }

    /// Where the root says its root slot's workspace was removed ([`REMOVED_FILE`]).
    pub fn removed_marker(&self) -> PathBuf {
        self.root().data_dir.join(REMOVED_FILE)
    }

    /// Whether the root slot's workspace was removed (and not made again).
    pub fn root_removed(&self) -> bool {
        std::fs::symlink_metadata(self.removed_marker()).is_ok()
    }

    /// The ids of the root's workspaces that exist: the root slot's first (unless it was
    /// removed), then the others' by id.
    pub fn workspace_ids(&self) -> std::io::Result<Vec<String>> {
        let root = self.root();
        let mut ids = Vec::new();
        if !root.root_removed() {
            ids.push(DEFAULT_WORKSPACE.to_string());
        }
        ids.extend(root.named_workspaces()?);
        Ok(ids)
    }

    /// The root's workspaces that exist: the default first, then the others by name.
    pub fn workspaces(&self) -> std::io::Result<Vec<Entry>> {
        let root = self.root();
        let ids = root.workspace_ids()?;
        let default = root.default_id();
        let mut out: Vec<Entry> = ids
            .into_iter()
            .filter_map(|id| {
                let paths = root.workspace(&id).ok()?;
                Some(Entry {
                    name: paths.display_name(),
                    default: default.as_deref() == Some(id.as_str()),
                    id,
                    paths,
                })
            })
            .collect();
        out.sort_by(|a, b| b.default.cmp(&a.default).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    /// The id of the root's default workspace: the one [`DEFAULT_FILE`] names while it exists,
    /// else the root slot unless it was removed. `None`: there is none.
    pub fn default_id(&self) -> Option<String> {
        let root = self.root();
        if let Some(id) = read_name(&root.data_dir.join(DEFAULT_FILE)) {
            if id != DEFAULT_WORKSPACE {
                if root.workspace(&id).is_ok_and(|p| p.exists()) {
                    return Some(id);
                }
            } else if !root.root_removed() {
                return Some(id);
            }
        }
        (!root.root_removed()).then(|| DEFAULT_WORKSPACE.to_string())
    }

    /// The paths of the workspace `selector` names in this root: the one so named, else the
    /// one with that id ([`crate::workspaces`]). One that names none gives the paths it would
    /// have as an id, which do not exist ([`Paths::exists`]). Fails on a selector that is no
    /// valid name.
    pub fn find_workspace(&self, selector: &str) -> anyhow::Result<Paths> {
        if let Some(why) = workspace_name_error(selector) {
            return Err(anyhow!(why));
        }
        let root = self.root();
        if selector == DEFAULT_WORKSPACE {
            return Ok(root);
        }
        let by_id = root.workspace(selector)?;
        if by_id.data_dir.is_dir() {
            return Ok(by_id);
        }
        if !root.root_removed() && root.display_name() == selector {
            return Ok(root);
        }
        for id in root.named_workspaces().unwrap_or_default() {
            let p = root.workspace(&id)?;
            if p.display_name() == selector {
                return Ok(p);
            }
        }
        Ok(by_id)
    }

    /// The default workspace's paths ([`Paths::default_id`]); `None` when there is none.
    pub fn default_workspace(&self) -> Option<Paths> {
        let id = self.default_id()?;
        self.root().workspace(&id).ok()
    }
}

/// A name read from `file` (its first line, trimmed), when it is a valid workspace name.
fn read_name(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let name = text.lines().next()?.trim();
    workspace_name_error(name)
        .is_none()
        .then(|| name.to_string())
}

/// Writes `text` to `path` whole or not at all: to a private temporary file beside it, then
/// renamed over it.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    let base = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        base.unwrap_or_default(),
        std::process::id()
    ));
    let res = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// The registry lock ([`REGISTRY_LOCK`]), held until dropped.
pub struct RegistryLock {
    file: std::fs::File,
}

/// Unlocked when let go, not only closed: a process another thread starts meanwhile shares the
/// open file until it execs, and the lock stays on it while anyone has it open.
impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl RegistryLock {
    /// Takes the lock of `root`'s registry, waiting for another holder (they hold it while
    /// they check and write a few small files, and `twr ws rm` while its question waits): as
    /// it starts to wait it calls the hook [`on_registry_wait`] set, which says so. Makes the
    /// root's data directory, private, when there is none yet.
    pub fn take(root: &Paths) -> anyhow::Result<RegistryLock> {
        let root = root.root();
        crate::paths::ensure_private_dir(&root.data_dir)?;
        let path = root.data_dir.join(REGISTRY_LOCK);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                if let Some(hook) = WAIT_HOOK
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    hook();
                }
                file.lock()
                    .with_context(|| format!("locking {}", path.display()))?;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }
        Ok(RegistryLock { file })
    }
}

type WaitHook = Box<dyn Fn() + Send + Sync>;

/// What this process does when a change to the workspaces waits for another's
/// ([`RegistryLock::take`]); none at first.
static WAIT_HOOK: std::sync::RwLock<Option<WaitHook>> = std::sync::RwLock::new(None);

/// Sets what this process does when a workspace change has to wait for another process's
/// (`twr ws rm` asking its question in another terminal, say): `twr ws` prints that it waits,
/// the TUI says it in its status line, rather than seeming to hang.
pub fn on_registry_wait(hook: impl Fn() + Send + Sync + 'static) {
    *WAIT_HOOK
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
}

/// Why `name` can't be the name of workspace `own` (an id; `None`: a new workspace) in the
/// workspaces `all`: not a valid name, `default` for another than the root slot, or another
/// workspace's name or id. `None` when it can.
pub fn name_conflict(all: &[Entry], own: Option<&str>, name: &str) -> Option<String> {
    if let Some(why) = workspace_name_error(name) {
        return Some(why);
    }
    for e in all {
        if Some(e.id.as_str()) == own {
            continue;
        }
        if e.name == name {
            return Some(match own {
                None => format!("workspace {name} already exists"),
                Some(_) => format!("there is a workspace {name} already (`twr ws` lists them)"),
            });
        }
        if e.id == name {
            return Some(format!(
                "{name} is the original name of workspace {}, which it still answers to: \
                 pick another name (or rename {} back to {name})",
                e.name, e.name
            ));
        }
    }
    if name == DEFAULT_WORKSPACE && own.is_some_and(|o| o != DEFAULT_WORKSPACE) {
        return Some(format!(
            "only the workspace in twr's root, which `twr ws new {DEFAULT_WORKSPACE}` makes, \
             can be named {DEFAULT_WORKSPACE}"
        ));
    }
    None
}

/// Which workspace with its id `ws` is: the token it was made with ([`GENERATION_FILE`]);
/// for one made before twr kept one, its directory's device, inode and birth time (one made
/// again since has a token, so never the same). `None` when neither can be read.
pub fn generation(ws: &Paths) -> Option<String> {
    dir_generation(&ws.data_dir)
}

/// [`generation`] of the workspace whose data directory is `dir`.
pub fn dir_generation(dir: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    if let Ok(t) = std::fs::read_to_string(dir.join(GENERATION_FILE)) {
        return Some(t.trim().to_string());
    }
    let dir = std::fs::metadata(dir).ok()?;
    let born = dir
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Some(format!("before:{}:{}:{born}", dir.dev(), dir.ino()))
}

/// Writes a new token in `ws`'s data directory ([`GENERATION_FILE`]).
fn new_generation(ws: &Paths) -> anyhow::Result<()> {
    let file = ws.data_dir.join(GENERATION_FILE);
    write_atomic(&file, &format!("{}\n", ulid::Ulid::generate()))
        .with_context(|| format!("writing {}", file.display()))
}

/// What [`create`] made.
#[derive(Debug)]
pub struct Created {
    pub paths: Paths,
    /// It is the root slot, made again after it was removed.
    pub root_slot: bool,
    /// It became the default: there was none.
    pub made_default: bool,
}

/// Creates workspace `name` in `root`, under the registry lock: a data directory of its own
/// (private, like every data directory) named `name`, its id; or, for `default`, the root
/// slot again once it was removed (what its removal left half done is removed first). The
/// first workspace made while there is no default becomes the default. Callers refuse agents.
pub fn create(root: &Paths, name: &str) -> anyhow::Result<Created> {
    use std::os::unix::fs::DirBuilderExt;
    let root = root.root();
    let _lock = RegistryLock::take(&root)?;
    let all = root.workspaces()?;
    if name == DEFAULT_WORKSPACE {
        if !root.root_removed() {
            let shown = root.display_name();
            if shown == DEFAULT_WORKSPACE {
                bail!("workspace {DEFAULT_WORKSPACE} already exists");
            }
            bail!(
                "{DEFAULT_WORKSPACE} is the original name of workspace {shown}, which it still \
                 answers to: pick another name (or rename {shown} back to {DEFAULT_WORKSPACE})"
            );
        }
        if let Some(why) = name_conflict(&all, Some(DEFAULT_WORKSPACE), name) {
            bail!(why);
        }
        crate::paths::check_socket_path(&root.socket)?;
        // What a removal cut short left goes (`worktrees/` and `trash/` among it), which a
        // `twr runner` keeping its data here would use as its own.
        if let Some(why) = runner_spool_why(&root, "make the workspace again") {
            bail!(why);
        }
        clear_root_leftovers(&root)?;
        new_generation(&root)?;
        let had_default = root.default_id().is_some();
        std::fs::remove_file(root.removed_marker())
            .with_context(|| format!("removing {}", root.removed_marker().display()))?;
        return Ok(Created {
            paths: root,
            root_slot: true,
            made_default: !had_default,
        });
    }
    if let Some(why) = name_conflict(&all, None, name) {
        bail!(why);
    }
    let ws = root.workspace(name)?;
    crate::paths::check_socket_path(&ws.socket)?;
    // A record of the default naming this id is left from a workspace that went (a removal cut
    // short): the new one does not become the default by it.
    forget_default(&root, name).context("updating the default workspace")?;
    crate::paths::ensure_private_dir(&root.workspaces_dir())?;
    match std::fs::DirBuilder::new().mode(0o700).create(&ws.data_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("workspace {name} already exists")
        }
        Err(e) => return Err(e).with_context(|| format!("creating {}", ws.data_dir.display())),
    }
    new_generation(&ws)?;
    let made_default = root.default_id().is_none();
    if made_default {
        write_atomic(&root.data_dir.join(DEFAULT_FILE), &format!("{name}\n"))
            .with_context(|| "recording the default workspace")?;
    }
    Ok(Created {
        paths: ws,
        root_slot: false,
        made_default,
    })
}

/// What the root slot's removal left in the root's data directory when it stopped half way
/// (its own entries, a `.rm-default-*` directory): removed, before it is made again.
fn clear_root_leftovers(root: &Paths) -> anyhow::Result<()> {
    let aside = move_root_owned_aside(root)?;
    if let Some(aside) = aside {
        remove_tree(&aside).with_context(|| format!("removing {}", aside.display()))?;
    }
    for e in std::fs::read_dir(&root.data_dir)?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with(".rm-default-") && e.file_type().is_ok_and(|t| t.is_dir()) {
            // (The removal that left it may be deleting it at this moment.)
            remove_tree(&e.path()).with_context(|| format!("removing {}", e.path().display()))?;
        }
    }
    Ok(())
}

/// The spool of a `twr runner` keeping its data in the root's data directory (its default),
/// `runner-<box>.db`, if there is one.
pub fn runner_spool(root: &Paths) -> Option<String> {
    std::fs::read_dir(&root.root().data_dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("runner-") && n.ends_with(".db"))
}

/// Why `default` can't be removed or made again while a `twr runner`'s spool is in the
/// root's data directory (its default `--data-dir`), whether that runner runs or not: the
/// spool holds what it has not delivered to its hub, and its worktrees and trash there are
/// `default`'s directories too. What to do, then `doing`. `None`: no spool is there.
pub fn runner_spool_why(root: &Paths, doing: &str) -> Option<String> {
    let spool = runner_spool(root)?;
    let dir = root.root().data_dir;
    let name = spool
        .strip_prefix("runner-")
        .and_then(|s| s.strip_suffix(".db"))
        .unwrap_or(&spool);
    Some(format!(
        "a `twr runner` for box {name} keeps its data in {dir}: its spool {spool}, which holds \
         what it has not delivered to its hub yet (and the agents it started), and its \
         worktrees and trash, which are `default`'s directories too. If that runner runs, or \
         will again, start it with a `--data-dir` of its own once its hub has what it holds \
         (it delivers it when it connects); if it never will, deleting {} loses only what it \
         never delivered. Then {doing}",
        dir.join(&spool).display(),
        dir = dir.display(),
    ))
}

/// Deletes the directory `path` and what is in it, a removal's aside one: what another
/// process deletes at the same time (the removal that moved it, or `twr ws new default`
/// clearing leftovers) is not an error.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// The name a removal moves workspace `id`'s files aside to, `.rm-<id>-<pid>-<nanoseconds>`:
/// a name no workspace can have, and none that a removal cut short left (whose process id
/// may since be another's) is taken again.
pub fn aside_name(id: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(".rm-{id}-{}-{nanos}", std::process::id())
}

/// Whether `name`, an entry of the root's data directory, is the root slot's own: one of
/// [`ROOT_OWNED`] or [`ROOT_OWNED_PREFIXES`], or what a writer of those left half written (a
/// temporary file of the input history's, or of [`write_atomic`]'s). `tui-history.lock`
/// stays: the removal holds it as it goes (so a TUI's save waits, then finds it gone).
pub fn root_owned(name: &str) -> bool {
    ROOT_OWNED.contains(&name)
        || ROOT_OWNED_PREFIXES.iter().any(|p| name.starts_with(p))
        || (name.starts_with("tui-history.") && name.ends_with(".tmp"))
        || [NAME_FILE, GENERATION_FILE]
            .iter()
            .any(|f| name.starts_with(&format!(".{f}.tmp-")))
}

/// Moves the root slot's own entries of the root's data directory ([`root_owned`]) into a
/// new directory beside them, `.rm-default-…` ([`aside_name`]), which it returns (`None` when there were
/// none): renames within one directory, so none is ever half moved.
pub fn move_root_owned_aside(root: &Paths) -> anyhow::Result<Option<PathBuf>> {
    let root = root.root();
    let mut owned = Vec::new();
    for e in std::fs::read_dir(&root.data_dir)
        .with_context(|| format!("reading {}", root.data_dir.display()))?
    {
        let e = e?;
        let n = e.file_name().to_string_lossy().into_owned();
        if root_owned(&n) {
            owned.push(n);
        }
    }
    if owned.is_empty() {
        return Ok(None);
    }
    let aside = root.data_dir.join(aside_name(DEFAULT_WORKSPACE));
    crate::paths::ensure_private_dir(&aside)?;
    for n in owned {
        std::fs::rename(root.data_dir.join(&n), aside.join(&n))
            .with_context(|| format!("moving {} aside", root.data_dir.join(&n).display()))?;
    }
    Ok(Some(aside))
}

/// What [`rename`] did.
#[derive(Debug)]
pub struct Renamed {
    pub id: String,
    pub from: String,
    pub to: String,
}

/// Renames the workspace `selector` names to `to`, under the registry lock: writes its
/// [`NAME_FILE`] (removes it when `to` is its id). Its hub may run meanwhile: nothing it
/// uses moves. Callers refuse agents.
pub fn rename(root: &Paths, selector: &str, to: &str) -> anyhow::Result<Renamed> {
    rename_found(root, |root| root.find_workspace(selector), selector, to)
}

/// [`rename`] of the workspace whose id is `id`, never one only named so (the TUI's, which
/// knows its workspaces by id: one removed meanwhile is not taken for another).
pub fn rename_id(root: &Paths, id: &str, to: &str) -> anyhow::Result<Renamed> {
    rename_found(root, |root| root.workspace(id), id, to)
}

fn rename_found(
    root: &Paths,
    find: impl Fn(&Paths) -> anyhow::Result<Paths>,
    selector: &str,
    to: &str,
) -> anyhow::Result<Renamed> {
    let root = root.root();
    let _lock = RegistryLock::take(&root)?;
    let ws = find(&root)?;
    if !ws.exists() {
        bail!("there is no workspace {selector} (`twr ws` lists them)");
    }
    let id = ws.workspace_id().to_string();
    let from = ws.display_name();
    if from == to {
        bail!("workspace {from} is named {to} already");
    }
    let all = root.workspaces()?;
    if let Some(why) = name_conflict(&all, Some(&id), to) {
        bail!(why);
    }
    let file = ws.data_dir.join(NAME_FILE);
    if to == id {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", file.display())),
        }
    } else {
        write_atomic(&file, &format!("{to}\n"))
            .with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(Renamed {
        id,
        from,
        to: to.to_string(),
    })
}

/// Makes the workspace `selector` names the default, under the registry lock. Its id and its
/// name. Callers refuse agents.
pub fn set_default(root: &Paths, selector: &str) -> anyhow::Result<(String, String)> {
    set_default_found(root, |root| root.find_workspace(selector), selector)
}

/// [`set_default`] of the workspace whose id is `id` (the TUI's, as [`rename_id`]).
pub fn set_default_id(root: &Paths, id: &str) -> anyhow::Result<(String, String)> {
    set_default_found(root, |root| root.workspace(id), id)
}

fn set_default_found(
    root: &Paths,
    find: impl Fn(&Paths) -> anyhow::Result<Paths>,
    selector: &str,
) -> anyhow::Result<(String, String)> {
    let root = root.root();
    let _lock = RegistryLock::take(&root)?;
    let ws = find(&root)?;
    if !ws.exists() {
        bail!("there is no workspace {selector} (`twr ws` lists them)");
    }
    let id = ws.workspace_id().to_string();
    let file = root.data_dir.join(DEFAULT_FILE);
    if id == DEFAULT_WORKSPACE {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", file.display())),
        }
    } else {
        write_atomic(&file, &format!("{id}\n"))
            .with_context(|| format!("writing {}", file.display()))?;
    }
    Ok((id, ws.display_name()))
}

/// After workspace `id` was removed (under the registry lock): the default workspace's
/// record no longer names it, so a workspace made later with the same id does not become the
/// default by surprise. The default is then the root slot, when it exists.
pub fn forget_default(root: &Paths, id: &str) -> std::io::Result<()> {
    let file = root.root().data_dir.join(DEFAULT_FILE);
    if read_name(&file).as_deref() == Some(id) {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let root = Paths::with_home(dir.path());
        (dir, root)
    }

    fn names(root: &Paths) -> Vec<(String, String, bool)> {
        root.workspaces()
            .unwrap()
            .into_iter()
            .map(|e| (e.id, e.name, e.default))
            .collect()
    }

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn a_fresh_root_has_its_default_workspace_only() {
        let (_d, root) = home();
        assert_eq!(names(&root), [(s("default"), s("default"), true)]);
        assert_eq!(root.default_id().as_deref(), Some("default"));
        assert!(root.exists());
    }

    #[test]
    fn a_renamed_workspace_answers_to_both_names_and_keeps_its_paths() {
        let (_d, root) = home();
        let made = create(&root, "client-x").unwrap();
        assert!(!made.made_default);
        let before = made.paths.clone();
        rename(&root, "client-x", "acme").unwrap();
        let by_name = root.find_workspace("acme").unwrap();
        let by_id = root.find_workspace("client-x").unwrap();
        assert_eq!(by_name, before);
        assert_eq!(by_id, before);
        assert_eq!(by_name.socket, before.socket);
        assert_eq!(by_name.worktree_namespace(), before.worktree_namespace());
        assert_eq!(by_name.display_name(), "acme");
        // Its original name is its own: nothing else may take it, it may go back to it.
        assert!(create(&root, "client-x").is_err());
        rename(&root, "acme", "client-x").unwrap();
        assert!(!before.data_dir.join(NAME_FILE).exists());
        assert_eq!(before.display_name(), "client-x");
    }

    #[test]
    fn the_default_workspace_can_be_renamed_and_keeps_being_the_default() {
        let (_d, root) = home();
        create(&root, "beta").unwrap();
        rename(&root, "default", "main").unwrap();
        assert_eq!(
            names(&root),
            [
                (s("default"), s("main"), true),
                (s("beta"), s("beta"), false)
            ]
        );
        assert_eq!(root.find_workspace("main").unwrap(), root);
        assert_eq!(root.find_workspace("default").unwrap(), root);
        // `default` stays its alias: no other workspace can be named it.
        let e = rename(&root, "beta", "default").unwrap_err().to_string();
        assert!(e.contains("original name of workspace main"), "{e}");
        let e = create(&root, "default").unwrap_err().to_string();
        assert!(e.contains("original name of workspace main"), "{e}");
        let e = create(&root, "main").unwrap_err().to_string();
        assert!(e.contains("already"), "{e}");
    }

    #[test]
    fn names_are_checked_against_every_name_and_id() {
        let (_d, root) = home();
        create(&root, "a").unwrap();
        create(&root, "b").unwrap();
        assert!(rename(&root, "a", "b").is_err());
        assert!(rename(&root, "a", "Bad").is_err());
        assert!(rename(&root, "a", "a").is_err());
        assert!(rename(&root, "nope", "c").is_err());
        rename(&root, "a", "c").unwrap();
        // `a` is still a's alias; `c` is taken by name.
        assert!(rename(&root, "b", "a").is_err());
        assert!(rename(&root, "b", "c").is_err());
        assert!(create(&root, "c").is_err());
    }

    #[test]
    fn the_default_can_be_moved_and_falls_back_to_the_root_slot() {
        let (_d, root) = home();
        create(&root, "a").unwrap();
        set_default(&root, "a").unwrap();
        assert_eq!(root.default_id().as_deref(), Some("a"));
        assert_eq!(root.workspaces().unwrap()[0].id, "a");
        // A record naming a workspace that went counts for nothing.
        std::fs::remove_dir_all(root.workspace("a").unwrap().data_dir).unwrap();
        assert_eq!(root.default_id().as_deref(), Some("default"));
        set_default(&root, "default").unwrap();
        assert!(!root.data_dir.join(DEFAULT_FILE).exists());
    }

    #[test]
    fn a_removed_root_slot_is_no_workspace_and_comes_back_by_name() {
        let (_d, root) = home();
        create(&root, "a").unwrap();
        std::fs::create_dir_all(&root.data_dir).unwrap();
        std::fs::write(root.data_dir.join("twr.db"), "x").unwrap();
        std::fs::write(root.data_dir.join("orch-mcp-1.json"), "x").unwrap();
        std::fs::write(root.data_dir.join("keep.bak"), "x").unwrap();
        std::fs::create_dir_all(root.data_dir.join("accounts")).unwrap();
        write_atomic(&root.removed_marker(), "").unwrap();
        assert!(!root.exists());
        assert_eq!(root.default_id(), None);
        assert_eq!(names(&root), [(s("a"), s("a"), false)]);
        // Nothing to select without `-w`; `default` names none.
        assert!(root.default_workspace().is_none());
        assert!(!root.find_workspace("default").unwrap().exists());
        // Not while a runner keeps its data there: its worktrees and trash would go.
        std::fs::write(root.data_dir.join("runner-gpu.db"), "").unwrap();
        let e = create(&root, "default").unwrap_err().to_string();
        assert!(
            e.contains("a `twr runner` for box gpu keeps its data in")
                && e.contains("runner-gpu.db"),
            "{e}"
        );
        assert!(
            e.contains("deleting ") && e.ends_with("Then make the workspace again"),
            "{e}"
        );
        assert!(root.data_dir.join("twr.db").exists() && root.root_removed());
        std::fs::remove_file(root.data_dir.join("runner-gpu.db")).unwrap();
        // A half done removal's leftovers go when it is made again; the rest stays.
        let made = create(&root, "default").unwrap();
        assert!(made.root_slot && made.made_default);
        assert!(root.exists());
        assert!(!root.data_dir.join("twr.db").exists());
        assert!(!root.data_dir.join("orch-mcp-1.json").exists());
        assert!(root.data_dir.join("keep.bak").exists());
        assert!(root.data_dir.join("accounts").is_dir());
        assert!(root.workspace("a").unwrap().exists());
    }

    /// Deleting an aside directory another process deleted first is no error.
    #[test]
    fn an_aside_directory_gone_meanwhile_is_no_error() {
        let (d, _root) = home();
        let aside = d.path().join(".rm-default-1-2");
        std::fs::create_dir_all(aside.join("worktrees")).unwrap();
        remove_tree(&aside).unwrap();
        assert!(!aside.exists());
        remove_tree(&aside).unwrap();
    }

    /// A change that has to wait for another (one holding the registry lock, `twr ws rm`
    /// asking its question) says so through the hook as it starts to wait, then goes on once
    /// the other lets go.
    #[test]
    fn a_change_that_waits_for_another_says_so() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static WAITS: AtomicUsize = AtomicUsize::new(0);
        on_registry_wait(|| {
            WAITS.fetch_add(1, Ordering::SeqCst);
        });
        let (_d, root) = home();
        let held = RegistryLock::take(&root).unwrap();
        let before = WAITS.load(Ordering::SeqCst);
        let creating = {
            let root = root.clone();
            std::thread::spawn(move || create(&root, "a").map(|_| ()))
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while WAITS.load(Ordering::SeqCst) == before {
            assert!(std::time::Instant::now() < deadline, "never said it waits");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!root.workspace("a").unwrap().exists());
        drop(held);
        creating.join().unwrap().unwrap();
        assert!(root.workspace("a").unwrap().exists());
    }

    #[test]
    fn the_root_slots_own_files_include_half_written_ones_but_not_the_history_lock() {
        for own in [
            "twr.db",
            "orch-mcp-1.json",
            "tui-history",
            "tui-history.1234.tmp",
            ".workspace-name.tmp-99",
            ".workspace-generation.tmp-99",
            GENERATION_FILE,
        ] {
            assert!(root_owned(own), "{own}");
        }
        for shared in [
            "tui-history.lock",
            "twr.lock",
            "workspaces",
            "accounts",
            "job-slots",
            "tui-state.json",
            "twr.db.bak-mine",
            REMOVED_FILE,
            DEFAULT_FILE,
            REGISTRY_LOCK,
        ] {
            assert!(!root_owned(shared), "{shared}");
        }
    }

    #[test]
    fn the_first_workspace_made_without_a_default_becomes_it() {
        let (_d, root) = home();
        std::fs::create_dir_all(&root.data_dir).unwrap();
        write_atomic(&root.removed_marker(), "").unwrap();
        let made = create(&root, "solo").unwrap();
        assert!(made.made_default);
        assert_eq!(root.default_id().as_deref(), Some("solo"));
        let other = create(&root, "two").unwrap();
        assert!(!other.made_default);
    }

    #[test]
    fn forgetting_a_removed_default_leaves_others_alone() {
        let (_d, root) = home();
        create(&root, "a").unwrap();
        set_default(&root, "a").unwrap();
        forget_default(&root, "b").unwrap();
        assert_eq!(root.default_id().as_deref(), Some("a"));
        forget_default(&root, "a").unwrap();
        assert_eq!(root.default_id().as_deref(), Some("default"));
    }

    #[test]
    fn an_unreadable_or_invalid_name_file_shows_the_id() {
        let (_d, root) = home();
        let a = create(&root, "a").unwrap().paths;
        std::fs::write(a.data_dir.join(NAME_FILE), "Not A Name\n").unwrap();
        assert_eq!(a.display_name(), "a");
    }
}
