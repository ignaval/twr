//! Filesystem locations.
//!
//! `TWR_HOME=X` overrides everything: `X/config.toml`, `X/data/`, `X/hub.sock`.
//! Otherwise (XDG variables are honored when set):
//!
//! - config: `$XDG_CONFIG_HOME/twr/config.toml`, default `~/.config/twr/config.toml`
//! - data: `$XDG_DATA_HOME/twr/`, default `~/.local/share/twr/` (`twr.db`, `worktrees/`, `hub.log`)
//! - socket: `$XDG_RUNTIME_DIR/twr/hub.sock`, else `$XDG_STATE_HOME/twr/hub.sock`,
//!   default `~/.local/state/twr/hub.sock`
//!
//! That is the `default` workspace. A named workspace `W` (`--workspace W`, else
//! `$TWR_WORKSPACE`) is a hub of its own inside the same root: it shares the config file, keeps
//! its data in `<data>/workspaces/W/` and listens on `<socket dir>/ws/W.sock`.

use std::path::{Component, Path, PathBuf};

use anyhow::anyhow;

/// Longest Unix socket path, in bytes (`sun_path` is 108 bytes on Linux, 104 on macOS,
/// including the terminating NUL).
#[cfg(target_os = "linux")]
pub const MAX_SOCKET_PATH: usize = 107;
/// Longest Unix socket path, in bytes (`sun_path` is 108 bytes on Linux, 104 on macOS,
/// including the terminating NUL).
#[cfg(not(target_os = "linux"))]
pub const MAX_SOCKET_PATH: usize = 103;

/// Fails with a clear message when `path` is too long to bind or connect to as a Unix socket
/// (over [`MAX_SOCKET_PATH`] bytes), instead of the kernel's opaque "path must be shorter than
/// SUN_LEN" / `EINVAL`.
pub fn check_socket_path(path: &Path) -> anyhow::Result<()> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(anyhow!(
            "the hub socket path {} is {len} bytes; Unix socket paths can be at most \
             {MAX_SOCKET_PATH} bytes. Pass a shorter one with --socket (or `hub.socket` in the \
             config), or set TWR_HOME to a shorter directory",
            path.display()
        ));
    }
    Ok(())
}

/// The workspace whose paths are the root's own (the standard layout, or `TWR_HOME`'s).
pub const DEFAULT_WORKSPACE: &str = "default";

/// The variable that selects a workspace when `--workspace` is not given. The hub sets it for
/// what it runs, so an agent's own `twr` calls reach the hub of its workspace.
pub const WORKSPACE_ENV: &str = "TWR_WORKSPACE";

/// Longest workspace name, in bytes (it is part of the socket path).
pub const MAX_WORKSPACE_NAME: usize = 32;

/// Why `name` cannot name a workspace: 1 to [`MAX_WORKSPACE_NAME`] lowercase letters, digits
/// or `-`, starting and ending with a letter or digit. It becomes a directory, a socket file
/// name, the branch component `@<name>` and part of the orchestrator's working directory,
/// which Claude Code names its memory directory after with every other character turned into
/// `-`: so two names never share one (nor a directory on a case-insensitive file system).
pub fn workspace_name_error(name: &str) -> Option<String> {
    let rule = format!(
        "use 1-{MAX_WORKSPACE_NAME} of [a-z0-9-], starting and ending with a letter or digit"
    );
    let chars_ok = !name.is_empty()
        && name.len() <= MAX_WORKSPACE_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let ends_ok = name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !(chars_ok && ends_ok) {
        return Some(format!("invalid workspace name {name:?}: {rule}"));
    }
    if let Some(why) = crate::git::branch_component_error(&format!("@{name}")) {
        return Some(format!(
            "invalid workspace name {name:?}: it names the git branch component @{name}, and \
             it {why}"
        ));
    }
    None
}

/// `s` as one POSIX shell word, so a suggested command can be pasted as shown: as it is when
/// it holds only characters no shell treats specially (and does not start with `=`, which zsh
/// expands), else in single quotes (an embedded `'` as `'\''`).
pub fn sh_word(s: &str) -> String {
    let plain = !s.is_empty()
        && !s.starts_with('=')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+,:@=".contains(&b));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// `text` (a message, maybe the hub's) with each command it quotes as `` `twr ...` `` run
/// the way the client reading it selects its workspace: `selection` (` -w <name>`, or
/// ` --socket <path>`) after its `twr`, unless the command selects one itself, is `twr ws`
/// (about every workspace) or is `twr runner` (a runner belongs to no workspace, and its
/// commands refuse `-w` and `--socket`). Empty `selection`: as it is.
pub fn localize_commands(text: &str, selection: &str) -> String {
    if selection.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + selection.len());
    let mut rest = text;
    while let Some(at) = rest.find("`twr ") {
        let (before, from) = rest.split_at(at + "`twr".len());
        out.push_str(before);
        let args = &from[1..];
        let own = [
            "-w ",
            "--workspace",
            "--socket",
            "ws ",
            "ws`",
            "runner ",
            "runner`",
        ];
        if !own.iter().any(|o| args.starts_with(o)) {
            out.push_str(selection);
        }
        rest = from;
    }
    out.push_str(rest);
    out
}

/// What a client puts after `twr` in a command it shows (`` `twr -w a attach api` ``), so that
/// the command reaches the workspace it shows (`workspace`; `None`: `default`) from a shell
/// whose `TWR_WORKSPACE` is `inherited`: ` -w <name>` for a named one, ` -w default` when the
/// shell selects another (one that is not UTF-8 too), else nothing.
pub fn workspace_selection(workspace: Option<&str>, inherited: Option<&std::ffi::OsStr>) -> String {
    match workspace.filter(|w| *w != DEFAULT_WORKSPACE) {
        Some(w) => format!(" -w {w}"),
        None if inherited.is_some_and(|v| !v.is_empty() && v != DEFAULT_WORKSPACE) => {
            format!(" -w {DEFAULT_WORKSPACE}")
        }
        None => String::new(),
    }
}

/// Resolved twr locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Config file.
    pub config_file: PathBuf,
    /// Data directory.
    pub data_dir: PathBuf,
    /// Hub socket.
    pub socket: PathBuf,
    /// The named workspace these paths are for; `None` for the `default` workspace.
    pub workspace: Option<String>,
}

impl Paths {
    /// Resolves paths from the process environment, in the workspace `$TWR_WORKSPACE` names
    /// (else `default`).
    pub fn from_env() -> anyhow::Result<Paths> {
        Self::from_env_in(None)
    }

    /// Resolves paths from the process environment in `workspace` (`--workspace`), else in
    /// the one `$TWR_WORKSPACE` names, else in `default`. A `$TWR_WORKSPACE` that is not UTF-8
    /// is refused (when it counts): it names no workspace, and is not `default` either.
    pub fn from_env_in(workspace: Option<&str>) -> anyhow::Result<Paths> {
        if workspace.is_none() {
            if let Some(v) = std::env::var_os(WORKSPACE_ENV) {
                if v.to_str().is_none() {
                    return Err(anyhow!(
                        "{WORKSPACE_ENV} is not UTF-8 ({}): it names no workspace",
                        v.to_string_lossy()
                    ));
                }
            }
        }
        Self::resolve_in(|k| std::env::var(k).ok(), workspace)
    }

    /// Resolves paths using `env` to look up variables (testable without touching the
    /// process environment), in the workspace [`WORKSPACE_ENV`] names. Empty values count as
    /// unset.
    pub fn resolve(env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Paths> {
        Self::resolve_in(env, None)
    }

    /// [`Paths::resolve`] in `workspace` when given (it wins over [`WORKSPACE_ENV`]).
    pub fn resolve_in(
        env: impl Fn(&str) -> Option<String>,
        workspace: Option<&str>,
    ) -> anyhow::Result<Paths> {
        let name = match workspace {
            Some(w) => Some(w.to_string()),
            None => env(WORKSPACE_ENV).filter(|v| !v.is_empty()),
        };
        let root = Self::resolve_root(&env)?;
        match name {
            Some(name) => root.workspace(&name),
            None => Ok(root),
        }
    }

    /// The `default` workspace's paths: `TWR_HOME`'s layout, else the standard one.
    fn resolve_root(env: &impl Fn(&str) -> Option<String>) -> anyhow::Result<Paths> {
        let var = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        if let Some(home) = var("TWR_HOME") {
            return Ok(Self::with_home(&home));
        }
        let home = var("HOME").ok_or_else(|| anyhow!("neither TWR_HOME nor HOME is set"))?;
        let config_dir = var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let data_home = var("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let socket_dir = match var("XDG_RUNTIME_DIR") {
            Some(run) => run.join("twr"),
            None => var("XDG_STATE_HOME")
                .unwrap_or_else(|| home.join(".local/state"))
                .join("twr"),
        };
        Ok(Paths {
            config_file: config_dir.join("twr").join("config.toml"),
            data_dir: data_home.join("twr"),
            socket: socket_dir.join("hub.sock"),
            workspace: None,
        })
    }

    /// Layout rooted at a single directory (what `TWR_HOME` selects; used by tests).
    pub fn with_home(home: &Path) -> Paths {
        Paths {
            config_file: home.join("config.toml"),
            data_dir: home.join("data"),
            socket: home.join("hub.sock"),
            workspace: None,
        }
    }

    /// The paths of workspace `name` in this root: `default` is the root's own; any other
    /// shares the root's config file and has its data in `<data>/workspaces/<name>/` and its
    /// socket at `<socket dir>/ws/<name>.sock`. Fails on a name [`workspace_name_error`]
    /// refuses. Whether the workspace exists is not checked here ([`Paths::exists`]).
    pub fn workspace(&self, name: &str) -> anyhow::Result<Paths> {
        if let Some(why) = workspace_name_error(name) {
            return Err(anyhow!(why));
        }
        let root = self.root();
        if name == DEFAULT_WORKSPACE {
            return Ok(root);
        }
        let socket_dir = root.socket.parent().unwrap_or(Path::new("/"));
        Ok(Paths {
            config_file: root.config_file.clone(),
            data_dir: root.workspaces_dir().join(name),
            socket: socket_dir.join("ws").join(format!("{name}.sock")),
            workspace: Some(name.to_string()),
        })
    }

    /// The `default` workspace's paths of this root.
    pub fn root(&self) -> Paths {
        if self.workspace.is_none() {
            return self.clone();
        }
        // `<root data>/workspaces/<name>` and `<socket dir>/ws/<name>.sock`, as built by
        // `workspace`.
        let up2 = |p: &Path| p.parent().and_then(Path::parent).map(Path::to_path_buf);
        Paths {
            config_file: self.config_file.clone(),
            data_dir: up2(&self.data_dir).unwrap_or_default(),
            socket: up2(&self.socket).unwrap_or_default().join("hub.sock"),
            workspace: None,
        }
    }

    /// The workspace's name: its own, or [`DEFAULT_WORKSPACE`].
    pub fn workspace_name(&self) -> &str {
        self.workspace.as_deref().unwrap_or(DEFAULT_WORKSPACE)
    }

    /// Where the root keeps its named workspaces: `<root data>/workspaces`.
    pub fn workspaces_dir(&self) -> PathBuf {
        self.root().data_dir.join("workspaces")
    }

    /// Whether the workspace exists: `default` always does; a named one once its directory
    /// was made (`twr ws new`).
    pub fn exists(&self) -> bool {
        self.workspace.is_none() || self.data_dir.is_dir()
    }

    /// The root's named workspaces, sorted: the directories under [`Paths::workspaces_dir`]
    /// whose names are valid workspace names (anything else there is not a workspace).
    pub fn named_workspaces(&self) -> std::io::Result<Vec<String>> {
        let entries = match std::fs::read_dir(self.workspaces_dir()) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut names = Vec::new();
        for e in entries {
            let e = e?;
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if name != DEFAULT_WORKSPACE
                && workspace_name_error(&name).is_none()
                && e.file_type()?.is_dir()
            {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// The orchestrator's working directory in a named workspace,
    /// `<workspace data>/orchestrator`: its own, so a Claude Code orchestrator keeps its own
    /// memories (Claude Code keys them by working directory). `None` in `default`, which uses
    /// `orchestrator.cwd` (else the home directory).
    pub fn orchestrator_dir(&self) -> Option<PathBuf> {
        self.workspace
            .as_ref()
            .map(|_| self.data_dir.join("orchestrator"))
    }

    /// The instance's display name: a named workspace's name; else a `TWR_HOME` layout uses
    /// its directory's name, and the standard XDG layout is `default`.
    pub fn instance_name(&self) -> String {
        if let Some(w) = &self.workspace {
            return w.clone();
        }
        let Some(home) = self.instance_home() else {
            return "default".into();
        };
        let home = resolve_lenient(home).unwrap_or_else(|| home.to_path_buf());
        home.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| home.display().to_string())
    }

    /// Branch namespace for an isolated instance. Directory names alone can repeat, so
    /// use the resolved data path. Existing threads keep their recorded branches.
    ///
    /// A named workspace adds `@<name>` to its root's: `@<name>`, or `<hash>/@<name>` under
    /// `TWR_HOME`. A thread name never starts with `@`, so the branch `twr/<thread>` of a
    /// thread of the root can never be the directory a workspace's branches need.
    pub fn worktree_namespace(&self) -> Option<String> {
        use sha2::{Digest, Sha256};
        if let Some(w) = &self.workspace {
            return Some(match self.root().worktree_namespace() {
                Some(root) => format!("{root}/@{w}"),
                None => format!("@{w}"),
            });
        }
        self.instance_home()?;
        let data = resolve_lenient(&self.data_dir).unwrap_or_else(|| self.data_dir.clone());
        Some(hex::encode(Sha256::digest(data.as_os_str().as_encoded_bytes()))[..16].to_string())
    }

    fn instance_home(&self) -> Option<&Path> {
        let home = self.config_file.parent()?;
        (self.data_dir == home.join("data")).then_some(home)
    }

    /// The boxes twr added itself (`twr box add`): `boxes.toml` next to the config file,
    /// shared by every workspace of the root as the config file is
    /// ([`crate::config::BoxesFile`]).
    pub fn boxes_file(&self) -> PathBuf {
        crate::config::BoxesFile::path_for(&self.config_file)
    }

    /// SQLite database file.
    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("twr.db")
    }

    /// Root of per-thread git worktrees.
    pub fn worktrees_dir(&self) -> PathBuf {
        self.data_dir.join("worktrees")
    }

    /// Log file of a detached hub.
    pub fn hub_log(&self) -> PathBuf {
        self.data_dir.join("hub.log")
    }
}

/// This process's effective user id (`geteuid(2)`).
pub fn effective_uid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { geteuid() }
}

/// Makes `dir` a private directory (mode 0700) of this user's: creates it (missing parents
/// get the default mode) or restricts an existing one that group or others can access, with a
/// warning since its contents may have been readable. The data directory holds every prompt,
/// transcript and answer, and the orchestrator's MCP configuration (which names the command
/// its runtime starts), so no other user may read it or replace what is in it, whatever the
/// umask.
///
/// Checked on the directory as opened ([`open_dir`]: `fstat`, then `fchmod` on the same
/// descriptor), so the directory checked is the one restricted. Refused: a directory another
/// user owns (they could replace what twr keeps there, however its mode is set), one that
/// cannot be made 0700, and anything that is not a directory. A symbolic link is followed:
/// what it names is checked. So is every directory on its way ([`check_ancestors`]): another
/// user who may rename one could put the whole path somewhere else after it was checked.
pub fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let mut created = false;
    if std::fs::symlink_metadata(dir).is_err() {
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("creating {}: {e}", parent.display()))?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => created = true,
            // Another process created it meanwhile: checked below like any existing one.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(anyhow!("creating {}: {e}", dir.display())),
        }
    }
    let f = open_dir(dir)?;
    let meta = f
        .metadata()
        .map_err(|e| anyhow!("reading {}: {e}", dir.display()))?;
    let mode = meta.mode() & 0o777;
    let restrict = match private_dir_verdict(meta.uid(), mode, effective_uid()) {
        Err(why) => return Err(anyhow!("{} {why}", dir.display())),
        Ok(restrict) => restrict,
    };
    check_ancestors(dir)?;
    if !restrict {
        return Ok(());
    }
    f.set_permissions(std::fs::Permissions::from_mode(0o700))
        .map_err(|e| {
            anyhow!(
                "{} (mode {mode:o}) is accessible by other users and cannot be restricted to \
                 its owner: {e}",
                dir.display()
            )
        })?;
    // Only the umask can loosen a directory we just created with 0700; nothing to report.
    if !created {
        tracing::warn!(
            dir = %dir.display(),
            mode = format!("{mode:o}"),
            "the data directory was accessible by other users; restricted it to its owner (0700)"
        );
    }
    Ok(())
}

/// Whether a directory owned by `uid` with permission bits `mode` can be this user's (`me`)
/// private directory ([`ensure_private_dir`]): `Ok(true)` when it can once restricted to its
/// owner (group or others can access it now), `Ok(false)` when it is already, `Err` (why not,
/// to follow its path) when another user owns it.
fn private_dir_verdict(uid: u32, mode: u32, me: u32) -> Result<bool, String> {
    if uid != me {
        return Err(format!(
            "belongs to another user (uid {uid}, twr runs as uid {me}): twr keeps its data only \
             in a directory of its own user's, since that user could replace what is in it. \
             Remove it, or point twr elsewhere (TWR_HOME, XDG_DATA_HOME)"
        ));
    }
    Ok(mode & 0o077 != 0)
}

/// Opens directory `dir` to check it on the descriptor (`O_DIRECTORY`, and `O_NONBLOCK` so a
/// FIFO put at its path is refused at once instead of blocking the open). Follows a symbolic
/// link.
fn open_dir(dir: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK)
        .open(dir)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ENOTDIR) => anyhow!("{} is not a directory", dir.display()),
            _ => anyhow!("opening {}: {e}", dir.display()),
        })
}

/// Refuses directory `dir` for the hub socket when another user could replace what twr puts in
/// it (the socket, or the private directory it is first bound in): one another user owns
/// (root excepted), or one every user may write to without the sticky bit (`/tmp` has it:
/// there only an entry's owner may rename or remove it). One its group may write to without
/// the sticky bit is used with a warning: with a group of the user's own (a umask of `002`
/// makes such directories) no one else can, and twr cannot tell who else is in a group.
/// Checked on the opened directory.
pub fn check_socket_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = open_dir(dir)?
        .metadata()
        .map_err(|e| anyhow!("reading {}: {e}", dir.display()))?;
    check_ancestors(dir)?;
    match socket_dir_verdict(meta.uid(), meta.mode(), effective_uid()) {
        SocketDir::Refused(why) => Err(anyhow!(
            "{} {why}: another user could replace the hub's socket there. Use a directory of \
             your own (--socket, `hub.socket`)",
            dir.display()
        )),
        SocketDir::GroupWritable => {
            tracing::warn!(
                dir = %dir.display(),
                gid = meta.gid(),
                mode = format!("{:o}", meta.mode() & 0o7777),
                "the hub socket's directory is writable by its group: any other member of that \
                 group could replace the socket there"
            );
            Ok(())
        }
        SocketDir::Private => Ok(()),
    }
}

/// Refuses `dir` when another user could make its path name something else after it was
/// checked: every directory the path is resolved through, as the kernel resolves it (from `/`,
/// each component in turn, each symbolic link met followed where it points, `..` after it),
/// must be one only this user or root can change the entries of. Refused: such a directory
/// another user owns (root excepted), or one every user may write to without the sticky bit;
/// a symbolic link another user owns (root excepted: in a directory with the sticky bit, such
/// as `/tmp`, only its owner may replace it). A directory its group may write to is logged,
/// as for the socket's own directory ([`check_socket_dir`]), once per process. The rules OpenSSH applies to the path of
/// its files, with each symbolic link's own directory checked as well. `dir` itself is its
/// callers' to check.
pub fn check_ancestors(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    /// Symbolic links followed at most, as Linux's `MAXSYMLINKS`.
    const MAX_LINKS: usize = 40;
    let me = effective_uid();
    let abs = std::path::absolute(dir).map_err(|e| anyhow!("resolving {}: {e}", dir.display()))?;
    let refuse = |what: &Path, why: String| {
        anyhow!(
            "{}, on the way to {}, {why}: another user could make that path name a directory of \
             theirs. Use a directory whose path only you (or root) can change",
            what.display(),
            dir.display()
        )
    };
    // Components left to resolve, the next one last.
    let mut left: Vec<std::ffi::OsString> = Vec::new();
    let push = |left: &mut Vec<std::ffi::OsString>, p: &Path| {
        for c in p.components().rev() {
            match c {
                Component::Normal(n) => left.push(n.to_os_string()),
                Component::ParentDir => left.push("..".into()),
                Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            }
        }
    };
    push(&mut left, &abs);
    let mut at = PathBuf::from("/");
    let mut links = 0;
    while let Some(name) = left.pop() {
        if name == ".." {
            at.pop();
            continue;
        }
        // The directory `name` is looked up in.
        let held = std::fs::metadata(&at).map_err(|e| anyhow!("reading {}: {e}", at.display()))?;
        match socket_dir_verdict(held.uid(), held.mode(), me) {
            SocketDir::Refused(why) => return Err(refuse(&at, why)),
            SocketDir::GroupWritable if first_group_warning(&at) => {
                #[cfg(test)]
                tests::GROUP_LOGGED.with(|n| n.set(n.get() + 1));
                tracing::warn!(
                    dir = %dir.display(),
                    ancestor = %at.display(),
                    gid = held.gid(),
                    "a directory on the way to twr's is writable by its group: any other member \
                     of that group could replace what is in it"
                );
            }
            SocketDir::GroupWritable => {}
            SocketDir::Private => {}
        }
        let next = at.join(&name);
        let meta = match std::fs::symlink_metadata(&next) {
            Ok(m) => m,
            // What does not exist yet is made by the caller, in the directory checked.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                at = next;
                continue;
            }
            Err(e) => return Err(anyhow!("reading {}: {e}", next.display())),
        };
        if !meta.file_type().is_symlink() {
            at = next;
            continue;
        }
        if meta.uid() != me && meta.uid() != 0 {
            return Err(refuse(
                &next,
                format!("is a symbolic link of another user's (uid {})", meta.uid()),
            ));
        }
        links += 1;
        if links > MAX_LINKS {
            return Err(anyhow!(
                "resolving {}: too many symbolic links",
                dir.display()
            ));
        }
        let target =
            std::fs::read_link(&next).map_err(|e| anyhow!("reading {}: {e}", next.display()))?;
        if target.is_absolute() {
            at = PathBuf::from("/");
        }
        push(&mut left, &target);
    }
    Ok(())
}

/// The group-writable directories [`check_ancestors`] has warned about, each logged once per
/// process: it runs on every job start, lock retry and history save, and a umask of `002`
/// makes `~/.local` and `~/.local/share` such directories.
static GROUP_WARNED: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Whether group-writable directory `dir`, on the way to one of twr's, is met for the first
/// time in this process ([`GROUP_WARNED`]).
fn first_group_warning(dir: &Path) -> bool {
    GROUP_WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(dir.to_path_buf())
}

/// What [`check_socket_dir`] (and [`check_ancestors`], for each directory on the way) makes of
/// a directory.
#[derive(Debug, PartialEq, Eq)]
enum SocketDir {
    /// No other user may replace what is in it.
    Private,
    /// Its group may write to it, and it has no sticky bit.
    GroupWritable,
    /// Not for the socket, and why.
    Refused(String),
}

/// What a directory owned by `uid` with mode `mode` (the sticky bit included) is for this
/// user's (`me`) hub socket ([`check_socket_dir`]).
fn socket_dir_verdict(uid: u32, mode: u32, me: u32) -> SocketDir {
    const STICKY: u32 = 0o1000;
    if uid != me && uid != 0 {
        return SocketDir::Refused(format!("belongs to another user (uid {uid})"));
    }
    if mode & STICKY != 0 {
        return SocketDir::Private;
    }
    if mode & 0o002 != 0 {
        return SocketDir::Refused(format!(
            "is writable by every user (mode {:o}) and has no sticky bit",
            mode & 0o7777
        ));
    }
    if mode & 0o020 != 0 {
        return SocketDir::GroupWritable;
    }
    SocketDir::Private
}

/// Restricts the existing file `path` to its owner (mode 0600) when group or others can
/// access it. A missing file is fine.
pub fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Opens `path` for appending, creating it with mode 0600 (and restricting an existing file
/// others can read): for the hub log, which repeats what the hub records.
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    restrict_file(path)?;
    Ok(f)
}

/// The directory identity of `p` for comparisons that decide safety (reservations, "inside a
/// worktree"): absolute, resolved with the filesystem's semantics as far as it exists, then
/// the rest appended as written.
///
/// Existing components resolve like the kernel resolves them: a symlink is followed before a
/// later `..` applies, so `alias/..` is the parent of alias's *target*, never the directory
/// holding `alias` (collapsing `..` first would name the wrong directory). From the first
/// component that does not exist on, components are appended (`.` dropped).
///
/// `None` when the identity cannot be established: a `..` after a missing component (what it
/// names depends on what that component becomes, e.g. a symlink), a dangling or looping
/// symlink, or a lookup error other than absence (a directory this user cannot search, I/O).
/// Callers fail closed on `None`.
pub fn resolve_lenient(p: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(p).ok()?;
    if let Ok(canon) = std::fs::canonicalize(&abs) {
        return Some(canon);
    }
    let mut out = PathBuf::new();
    let mut missing = false;
    for c in abs.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            // `out` is resolved (no symlinks) while nothing is missing: its lexical parent is
            // its real parent.
            Component::ParentDir if missing => return None,
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) if missing => out.push(name),
            Component::Normal(name) => {
                let next = out.join(name);
                match std::fs::symlink_metadata(&next) {
                    Ok(m) if m.file_type().is_symlink() => {
                        out = std::fs::canonicalize(&next).ok()?
                    }
                    Ok(_) => out = next,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        missing = true;
                        out = next;
                    }
                    Err(_) => return None,
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn twr_home_overrides_everything() {
        let p = Paths::resolve(env(&[
            ("TWR_HOME", "/tmp/th"),
            ("HOME", "/home/u"),
            ("XDG_RUNTIME_DIR", "/run/user/1"),
        ]))
        .unwrap();
        assert_eq!(p.config_file, PathBuf::from("/tmp/th/config.toml"));
        assert_eq!(p.data_dir, PathBuf::from("/tmp/th/data"));
        assert_eq!(p.socket, PathBuf::from("/tmp/th/hub.sock"));
        assert_eq!(p.db_file(), PathBuf::from("/tmp/th/data/twr.db"));
        assert_eq!(p.worktrees_dir(), PathBuf::from("/tmp/th/data/worktrees"));
        assert_eq!(p.hub_log(), PathBuf::from("/tmp/th/data/hub.log"));
    }

    #[test]
    fn isolated_instance_names_and_namespaces_follow_the_home() {
        let a = Paths::with_home(Path::new("/tmp/a/work"));
        let b = Paths::with_home(Path::new("/tmp/b/work"));
        assert_eq!(a.instance_name(), "work");
        assert_eq!(a.instance_name(), b.instance_name());
        assert_ne!(a.worktree_namespace(), b.worktree_namespace());
        assert_eq!(
            a.worktree_namespace(),
            Paths::with_home(Path::new("/tmp/a/./work")).worktree_namespace()
        );
        let default = Paths::resolve(env(&[("HOME", "/tmp/user")])).unwrap();
        assert_eq!(default.instance_name(), "default");
        assert!(default.worktree_namespace().is_none());
    }

    #[test]
    fn instance_names_resolve_relative_parent_root_and_symlink_paths() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            Paths::with_home(Path::new(".")).instance_name(),
            cwd.file_name().unwrap().to_string_lossy()
        );
        assert_eq!(
            Paths::with_home(Path::new(".")).worktree_namespace(),
            Paths::with_home(&cwd).worktree_namespace()
        );
        assert_eq!(Paths::with_home(Path::new("/")).instance_name(), "/");
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("work");
        std::fs::create_dir(&home).unwrap();
        assert_eq!(
            Paths::with_home(&home.join("..")).instance_name(),
            dir.path().file_name().unwrap().to_string_lossy()
        );
        // A parent after a missing component has no established filesystem identity.
        // Show its path rather than mistaking it for the default instance.
        let missing = dir.path().join("missing/..");
        assert_eq!(
            Paths::with_home(&missing).instance_name(),
            missing.display().to_string()
        );
        std::os::unix::fs::symlink(&home, dir.path().join("link")).unwrap();
        let real = Paths::with_home(&home);
        let link = Paths::with_home(&dir.path().join("link"));
        assert_eq!(real.instance_name(), link.instance_name());
        assert_eq!(real.worktree_namespace(), link.worktree_namespace());
        assert_eq!(
            real.instance_name(),
            Paths::with_home(&home.join(".")).instance_name()
        );
    }

    #[test]
    fn defaults_from_home() {
        let p = Paths::resolve(env(&[("HOME", "/home/u"), ("TWR_HOME", "")])).unwrap();
        assert_eq!(
            p.config_file,
            PathBuf::from("/home/u/.config/twr/config.toml")
        );
        assert_eq!(p.data_dir, PathBuf::from("/home/u/.local/share/twr"));
        assert_eq!(p.socket, PathBuf::from("/home/u/.local/state/twr/hub.sock"));
    }

    #[test]
    fn xdg_runtime_dir_for_socket() {
        let p = Paths::resolve(env(&[
            ("HOME", "/home/u"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_DATA_HOME", "/data"),
        ]))
        .unwrap();
        assert_eq!(p.socket, PathBuf::from("/run/user/1000/twr/hub.sock"));
        assert_eq!(p.config_file, PathBuf::from("/cfg/twr/config.toml"));
        assert_eq!(p.data_dir, PathBuf::from("/data/twr"));
    }

    #[test]
    fn socket_path_length_is_checked() {
        let ok = PathBuf::from(format!("/{}", "a".repeat(MAX_SOCKET_PATH - 1)));
        assert_eq!(ok.as_os_str().len(), MAX_SOCKET_PATH);
        assert!(check_socket_path(&ok).is_ok());
        let long = PathBuf::from(format!("/{}", "a".repeat(MAX_SOCKET_PATH)));
        let e = check_socket_path(&long).unwrap_err().to_string();
        assert!(e.contains(&format!("{} bytes", MAX_SOCKET_PATH + 1)), "{e}");
        assert!(e.contains("--socket") && e.contains("TWR_HOME"), "{e}");
    }

    #[test]
    fn named_workspaces_nest_in_the_standard_root() {
        let base = [("HOME", "/home/u"), ("XDG_RUNTIME_DIR", "/run/user/1000")];
        let root = Paths::resolve(env(&base)).unwrap();
        let w = Paths::resolve_in(env(&base), Some("client-x")).unwrap();
        assert_eq!(
            w.config_file, root.config_file,
            "one config for every workspace"
        );
        assert_eq!(
            w.data_dir,
            PathBuf::from("/home/u/.local/share/twr/workspaces/client-x")
        );
        assert_eq!(
            w.socket,
            PathBuf::from("/run/user/1000/twr/ws/client-x.sock")
        );
        assert_eq!(w.db_file(), w.data_dir.join("twr.db"));
        assert_eq!(w.worktrees_dir(), w.data_dir.join("worktrees"));
        assert_eq!(w.workspace_name(), "client-x");
        assert_eq!(w.instance_name(), "client-x");
        assert_eq!(w.worktree_namespace().as_deref(), Some("@client-x"));
        assert_eq!(w.orchestrator_dir(), Some(w.data_dir.join("orchestrator")));
        assert_eq!(w.root(), root);
        assert_eq!(w.workspaces_dir(), root.workspaces_dir());
        assert_eq!(root.workspace_name(), "default");
        assert_eq!(root.orchestrator_dir(), None);
        // `default` is the root itself.
        assert_eq!(
            Paths::resolve_in(env(&base), Some("default")).unwrap(),
            root
        );
        assert_eq!(root.workspace("default").unwrap(), root);
        assert_eq!(w.workspace("default").unwrap(), root);
        assert_eq!(w.workspace("client-x").unwrap(), w);
    }

    #[test]
    fn the_workspace_variable_selects_and_the_flag_wins() {
        let with_var = [("HOME", "/home/u"), ("TWR_WORKSPACE", "a")];
        assert_eq!(
            Paths::resolve(env(&with_var)).unwrap().workspace.as_deref(),
            Some("a")
        );
        assert_eq!(
            Paths::resolve_in(env(&with_var), Some("b"))
                .unwrap()
                .workspace
                .as_deref(),
            Some("b")
        );
        assert_eq!(
            Paths::resolve_in(env(&with_var), Some("default"))
                .unwrap()
                .workspace,
            None
        );
        let empty = [("HOME", "/home/u"), ("TWR_WORKSPACE", "")];
        assert_eq!(Paths::resolve(env(&empty)).unwrap().workspace, None);
        let bad = [("HOME", "/home/u"), ("TWR_WORKSPACE", "../x")];
        let e = Paths::resolve(env(&bad)).unwrap_err().to_string();
        assert!(e.contains("invalid workspace name"), "{e}");
        // Without the state and runtime variables the socket follows the state directory.
        let w = Paths::resolve_in(env(&[("HOME", "/home/u")]), Some("a")).unwrap();
        assert_eq!(
            w.socket,
            PathBuf::from("/home/u/.local/state/twr/ws/a.sock")
        );
    }

    #[test]
    fn workspaces_nest_in_a_twr_home() {
        let e = [("TWR_HOME", "/tmp/th"), ("HOME", "/home/u")];
        let root = Paths::resolve(env(&e)).unwrap();
        let w = Paths::resolve_in(env(&e), Some("a")).unwrap();
        assert_eq!(w.config_file, PathBuf::from("/tmp/th/config.toml"));
        assert_eq!(w.data_dir, PathBuf::from("/tmp/th/data/workspaces/a"));
        assert_eq!(w.socket, PathBuf::from("/tmp/th/ws/a.sock"));
        assert_eq!(w.root(), root);
        assert_eq!(w.instance_name(), "a");
        // The root's hash keeps two homes' workspaces of one name apart in a repository.
        let hash = root.worktree_namespace().unwrap();
        assert_eq!(w.worktree_namespace(), Some(format!("{hash}/@a")));
        let other = Paths::with_home(Path::new("/tmp/other"))
            .workspace("a")
            .unwrap();
        assert_ne!(other.worktree_namespace(), w.worktree_namespace());
    }

    #[test]
    fn workspace_names_are_checked() {
        for ok in [
            "a",
            "client-x",
            "a-b--c-1",
            "0",
            &"w".repeat(MAX_WORKSPACE_NAME),
        ] {
            assert_eq!(workspace_name_error(ok), None, "{ok}");
        }
        let long = "w".repeat(MAX_WORKSPACE_NAME + 1);
        for bad in [
            "", "A", "Work", "-a", "a-", ".a", "a.", "a/b", "../x", "a b", "a@b", "a..b", "x.lock",
            "client_x", "client.x", &long,
        ] {
            assert!(workspace_name_error(bad).is_some(), "{bad:?}");
            assert!(Paths::with_home(Path::new("/h")).workspace(bad).is_err());
        }
        // Every valid name is a valid branch component behind `@`.
        assert!(crate::git::branch_component_error("@client-x").is_none());
    }

    #[test]
    fn named_workspaces_are_the_valid_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = Paths::with_home(dir.path());
        assert_eq!(root.named_workspaces().unwrap(), Vec::<String>::new());
        let ws = root.workspaces_dir();
        for d in ["b", "a", "Bad", "default", "-x"] {
            std::fs::create_dir_all(ws.join(d)).unwrap();
        }
        std::fs::write(ws.join("file"), "").unwrap();
        assert_eq!(root.named_workspaces().unwrap(), ["a", "b"]);
        let a = root.workspace("a").unwrap();
        assert_eq!(
            a.named_workspaces().unwrap(),
            ["a", "b"],
            "listed from the root"
        );
        assert!(a.exists() && root.exists());
        assert!(!root.workspace("c").unwrap().exists());
    }

    #[test]
    fn commands_shown_reach_the_workspace() {
        let text = "no item in the trash (`twr trash list` shows them); `twr -w b ls` stays; \
                    `twr --socket /s info` too; `twr ws new a` and `twr ws` too; twr alone stays";
        assert_eq!(
            localize_commands(text, " -w a"),
            "no item in the trash (`twr -w a trash list` shows them); `twr -w b ls` stays; \
             `twr --socket /s info` too; `twr ws new a` and `twr ws` too; twr alone stays"
        );
        assert_eq!(localize_commands(text, ""), text);
        // A runner belongs to no workspace: its commands are shown as they run.
        let token = "too short: `twr runner token` prints a new one; `twr runner` too";
        assert_eq!(localize_commands(token, " -w a"), token);
        assert_eq!(localize_commands(token, " --socket /s"), token);
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(workspace_selection(Some("a"), None), " -w a");
        assert_eq!(workspace_selection(Some("default"), None), "");
        assert_eq!(
            workspace_selection(None, Some(OsStr::new("a"))),
            " -w default"
        );
        assert_eq!(workspace_selection(None, Some(OsStr::new("default"))), "");
        assert_eq!(workspace_selection(None, Some(OsStr::new(""))), "");
        assert_eq!(
            workspace_selection(None, Some(OsStr::from_bytes(b"a\xff"))),
            " -w default"
        );
        assert_eq!(
            workspace_selection(Some("b"), Some(OsStr::new("a"))),
            " -w b"
        );
    }

    #[test]
    fn no_home_is_an_error() {
        assert!(Paths::resolve(env(&[])).is_err());
    }

    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn chmod(p: &Path, m: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }

    #[test]
    fn private_dirs_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("pub");
        std::fs::create_dir(&parent).unwrap();
        chmod(&parent, 0o755);
        // Created: 0700, the parent is left alone.
        let fresh = parent.join("a").join("data");
        ensure_private_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        assert_eq!(mode(&parent), 0o755);
        // Existing and public: restricted.
        let old = parent.join("old");
        std::fs::create_dir(&old).unwrap();
        chmod(&old, 0o755);
        ensure_private_dir(&old).unwrap();
        assert_eq!(mode(&old), 0o700);
        // Not a directory.
        let file = parent.join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(ensure_private_dir(&file).is_err());

        // The hub log: created 0600, an existing public one restricted.
        let log = fresh.join("hub.log");
        drop(open_private_append(&log).unwrap());
        assert_eq!(mode(&log), 0o600);
        chmod(&log, 0o644);
        drop(open_private_append(&log).unwrap());
        assert_eq!(mode(&log), 0o600);
        restrict_file(&fresh.join("missing")).unwrap();
    }

    /// A directory another user owns is refused (here `/`, root's, when the tests do not run
    /// as root), and so is anything that is not a directory, a FIFO too (without blocking on
    /// it); a symbolic link to one of this user's directories is followed.
    #[test]
    fn a_private_dir_is_this_users_and_a_directory() {
        assert_eq!(private_dir_verdict(1000, 0o700, 1000), Ok(false));
        assert_eq!(private_dir_verdict(1000, 0o755, 1000), Ok(true));
        assert_eq!(private_dir_verdict(1000, 0o710, 1000), Ok(true));
        let e = private_dir_verdict(1001, 0o700, 1000).unwrap_err();
        assert!(e.starts_with("belongs to another user (uid 1001"), "{e}");
        assert!(private_dir_verdict(0, 0o700, 1000).is_err());
        if effective_uid() != 0 {
            let e = ensure_private_dir(Path::new("/")).unwrap_err().to_string();
            assert!(e.contains("belongs to another user (uid 0"), "{e}");
        }
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: mkfifo reads the NUL-terminated path it is given.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let e = ensure_private_dir(&fifo).unwrap_err().to_string();
        assert!(e.ends_with("is not a directory"), "{e}");
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        chmod(&real, 0o750);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        ensure_private_dir(&link).unwrap();
        assert_eq!(mode(&real), 0o700);
    }

    /// The socket's directory: this user's or root's, and writable by no other user unless it
    /// has the sticky bit (`/tmp`); one its group may write to is used (with a warning).
    #[test]
    fn a_socket_dir_others_can_write_needs_the_sticky_bit() {
        use SocketDir::*;
        assert_eq!(socket_dir_verdict(1000, 0o40700, 1000), Private);
        assert_eq!(socket_dir_verdict(1000, 0o40755, 1000), Private);
        assert_eq!(socket_dir_verdict(0, 0o41777, 1000), Private);
        assert_eq!(socket_dir_verdict(0, 0o40755, 1000), Private);
        assert_eq!(socket_dir_verdict(1000, 0o41770, 1000), Private);
        assert_eq!(socket_dir_verdict(1000, 0o40775, 1000), GroupWritable);
        assert_eq!(
            socket_dir_verdict(1000, 0o40777, 1000),
            Refused("is writable by every user (mode 777) and has no sticky bit".into())
        );
        assert_eq!(
            socket_dir_verdict(1001, 0o40700, 1000),
            Refused("belongs to another user (uid 1001)".into())
        );
        assert!(matches!(
            socket_dir_verdict(1001, 0o41777, 1000),
            Refused(_)
        ));
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("s");
        std::fs::create_dir(&d).unwrap();
        for (m, ok) in [
            (0o700, true),
            (0o755, true),
            (0o775, true),
            (0o1777, true),
            (0o777, false),
            (0o757, false),
        ] {
            chmod(&d, m);
            assert_eq!(check_socket_dir(&d).is_ok(), ok, "{m:o}");
        }
        chmod(&d, 0o777);
        let e = check_socket_dir(&d).unwrap_err().to_string();
        assert!(
            e.contains("another user could replace the hub's socket"),
            "{e}"
        );
        chmod(&d, 0o700);
    }

    /// A directory on the way that every user may write to without the sticky bit makes the
    /// data and socket directories below it refused, also reached through a symbolic link
    /// from a safe place; one its group may write to, or one with the sticky bit, does not.
    #[test]
    fn a_directory_on_the_way_others_can_rename_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let open = tmp.path().join("open");
        let data = open.join("b").join("data");
        std::fs::create_dir_all(data.parent().unwrap()).unwrap();
        chmod(&open, 0o757);
        let e = ensure_private_dir(&data).unwrap_err().to_string();
        assert!(e.contains("on the way to"), "{e}");
        assert!(e.contains("has no sticky bit"), "{e}");
        let e = check_socket_dir(data.parent().unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("on the way to"), "{e}");
        let safe = tmp.path().join("safe");
        std::fs::create_dir(&safe).unwrap();
        chmod(&safe, 0o700);
        let link = safe.join("link");
        std::os::unix::fs::symlink(data.parent().unwrap(), &link).unwrap();
        assert!(ensure_private_dir(&link.join("data")).is_err());
        for m in [0o1777, 0o775, 0o755] {
            chmod(&open, m);
            ensure_private_dir(&data).unwrap();
            check_socket_dir(data.parent().unwrap()).unwrap();
        }
        chmod(&open, 0o700);
    }

    thread_local! {
        /// The group-writable directories [`check_ancestors`] logged on this thread.
        pub(super) static GROUP_LOGGED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// A directory its group may write to, on the way to twr's, is logged the first time
    /// only: the check runs on every job start, lock retry and history save.
    #[test]
    fn a_group_writable_ancestor_is_logged_once() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        chmod(&shared, 0o775);
        let logged = || GROUP_LOGGED.with(std::cell::Cell::get);
        let before = logged();
        ensure_private_dir(&shared.join("data")).unwrap();
        let first = logged();
        assert!(
            first > before,
            "the group-writable directory was not logged"
        );
        ensure_private_dir(&shared.join("data")).unwrap();
        check_ancestors(&shared.join("data").join("jobs")).unwrap();
        check_ancestors(&shared.join("other")).unwrap();
        assert_eq!(logged(), first);
    }

    /// An alias for a safe private directory, put in a directory every user may write to
    /// without the sticky bit, is refused: anyone could replace it after the check. One this
    /// user owns in a directory with the sticky bit (as in `/tmp`) is followed and checked
    /// where it points; `..` after it applies to where it points.
    #[test]
    fn an_alias_in_a_directory_others_can_write_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let safe = tmp.path().join("safe");
        let data = safe.join("data");
        std::fs::create_dir_all(&data).unwrap();
        chmod(&safe, 0o700);
        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::os::unix::fs::symlink(&data, open.join("alias")).unwrap();
        chmod(&open, 0o777);
        let e = ensure_private_dir(&open.join("alias"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("has no sticky bit"), "{e}");
        assert!(check_socket_dir(&open.join("alias")).is_err());
        chmod(&open, 0o1777);
        ensure_private_dir(&open.join("alias")).unwrap();
        check_socket_dir(&open.join("alias")).unwrap();
        // `..` after the alias is the parent of what it names.
        ensure_private_dir(&open.join("alias").join("..").join("data")).unwrap();
        chmod(&open, 0o700);
    }

    /// A symlink is followed before a later `..` applies; a missing suffix is appended; what
    /// cannot be pinned down is `None`.
    #[test]
    fn paths_resolve_with_filesystem_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let wt = root.join("wt");
        std::fs::create_dir_all(wt.join("sub")).unwrap();
        std::os::unix::fs::symlink(wt.join("sub"), root.join("alias")).unwrap();
        let r = |p: PathBuf| resolve_lenient(&p);
        // `alias/..` is the worktree (alias's target's parent), not `root`.
        assert_eq!(r(root.join("alias/..")), Some(wt.clone()));
        assert_eq!(r(root.join("alias/../new/x")), Some(wt.join("new/x")));
        assert_eq!(
            r(root.join("alias/./missing")),
            Some(wt.join("sub/missing"))
        );
        assert_eq!(r(root.join("wt/sub/../sub")), Some(wt.join("sub")));
        // `..` after a missing component: it could become a symlink.
        assert_eq!(r(root.join("missing/../wt")), None);
        // Dangling and looping symlinks.
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling")).unwrap();
        assert_eq!(r(root.join("dangling/x")), None);
        std::os::unix::fs::symlink(root.join("loop"), root.join("loop")).unwrap();
        assert_eq!(r(root.join("loop")), None);
        assert_eq!(r(PathBuf::from("/..")), Some(PathBuf::from("/")));
    }

    /// A shell word is the string itself once the shell has read it: plain when nothing in it
    /// is special, quoted otherwise.
    #[test]
    fn sh_words_read_back_as_given() {
        assert_eq!(
            sh_word("/home/me/.config/twr/config.toml"),
            "/home/me/.config/twr/config.toml"
        );
        assert_eq!(sh_word("it's"), r"'it'\''s'");
        for s in [
            "", "a b", "$HOME", "it's", "=ls", "x%y", "~me", "a\nb", "*", "é",
        ] {
            let w = sh_word(s);
            let out = std::process::Command::new("sh")
                .args(["-c", &format!("printf %s {w}")])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), s, "{w}");
        }
    }
}
