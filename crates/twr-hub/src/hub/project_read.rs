//! The orchestrator's reads of project files: `project.files`, `project.read` and
//! `project.search` (its MCP tools `list_project_files`, `read_project_file` and
//! `search_project`), so it can answer "what does api's README say" without a worker.
//!
//! What it can read, and what it never can:
//!
//! - **Projects.** The ones [`Core::context_target`](super::Core) resolves (configured
//!   projects, and the projects of worker threads), on the hub's own box. A project on a
//!   remote box is refused, and so is one that is not a git repository, whose repository
//!   holds the home folder, or holds or is in twr's data directory, `TWR_HOME` or a
//!   credentials folder of the home folder ([`protected_dirs`], [`repo_refusal`]), or whose
//!   git directory is outside it (a `.git` file pointing elsewhere), or reads objects from
//!   elsewhere: a `commondir`, alternates, or a symbolic link anywhere in its object storage
//!   ([`git_dir_checks`], checked before and again after each call's git commands). Git
//!   runs in the repository's top level as git resolves it, symbolic links followed once, up
//!   front.
//! - **Committed content only.** Everything is read from git objects at one commit (the
//!   repository's `HEAD`, or the branch of one of the project's worker threads, `thread`),
//!   never from the work tree: untracked and ignored files (`.env`) do not exist there, a
//!   symbolic link is its target's name (never followed), a submodule is listed and never
//!   entered.
//! - **No secrets.** A path whose name looks like a secret (`.env`, keys, credentials, ...:
//!   [`secret_name`]; plus `[orchestrator] read_deny`) is refused, and so is a file whose
//!   content holds a private key or a known token format ([`secret_marker`]): the whole file,
//!   with the reason. A search returns a file's lines only once the whole file passed the
//!   checks a read makes, so a line of a key file never comes back even when it looks like
//!   nothing, and searches again without the files that failed, so whether they matched
//!   shows neither in its lines nor in `more` ([`Hub::search`]). A refusal never carries
//!   what git printed (`git_failed`), and is cut to 1 KB.
//! - **Sizes.** A file over [`READ_MAX`] or binary is refused; a read returns [`PAGE`] bytes
//!   with the offset of the rest; a listing at most [`LIST_MAX`] entries; a search at most
//!   [`SEARCH_HITS`] lines of at most [`SEARCH_LINE_MAX`] bytes.
//! - **A budget per turn.** One orchestrator turn makes at most [`TURN_CALLS`] calls and
//!   reads at most [`TURN_BYTES`] of content (`OrchState::take_read_call`,
//!   `take_read_bytes`), whatever started the turn: a worker report in a notice turn cannot
//!   have the orchestrator copy a repository wholesale.
//! - **On screen.** Every call of the orchestrator, served or refused, appends a
//!   `project.read` event to its thread (tool, project, path or query, branch, commit, bytes,
//!   refusal), which the TUI draws in its conversation and its transcript shows. Past the
//!   turn's calls only the first refusal is: they read nothing, and cannot flood the log.
//!
//! `[orchestrator] project_reads = false` refuses all of it. A user may call the methods too
//! (nothing is recorded, no budget); a worker's agent may not (`server::AGENT_METHODS`).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};
use twr_core::event::{EventBody, ProjectRead, Source};
use twr_core::git::{valid_read_path, EntryKind, GrepRequest, TreeEntry};
use twr_core::proto::{codes, RpcError};

use super::lifecycle::data_dir_of;
use super::orch::ReadKey;
use super::{git, internal, rpc, Caller, Hub, LOCAL_BOX};

/// Most project-read calls in one orchestrator turn.
pub const TURN_CALLS: u32 = 40;
/// Most bytes of content returned in one orchestrator turn.
pub const TURN_BYTES: u64 = 256 * 1024;
/// Largest file read, in bytes.
pub const READ_MAX: u64 = 1 << 20;
/// Most bytes of a file one read returns.
pub const PAGE: usize = 16 * 1024;
/// Most entries one listing returns.
pub const LIST_MAX: usize = 500;
/// Most lines one search returns.
pub const SEARCH_HITS: usize = 50;
/// Most lines of one file a search returns.
pub const SEARCH_PER_FILE: u32 = 5;
/// Longest search, in bytes.
pub const QUERY_MAX: usize = 200;
/// Longest line a search returns, in bytes.
pub const SEARCH_LINE_MAX: usize = 200;
/// A file with a NUL in its first this many bytes is binary.
const BINARY_PROBE: usize = 8 * 1024;
/// Longest symbolic link target read.
const LINK_MAX: u64 = 4096;
/// Longest path or query kept in a `project.read` event.
const RECORD_MAX: usize = 512;
/// Longest refusal returned and recorded, in bytes.
const REFUSAL_MAX: usize = 1024;
/// Longest project or thread name taken, in bytes.
const NAME_MAX: usize = 256;
/// Most lines git grep returns to the hub for one search (the content checks take some out).
const SEARCH_CANDIDATES: usize = SEARCH_HITS + 25;
/// Most times one search runs git grep: each time after the first leaves out files git found
/// whose content a read refuses.
const SEARCH_ROUNDS: usize = 8;
/// Most files with names that are not UTF-8 one search may cover (each is read first).
const UNNAMED_MAX: usize = 64;
/// Most files one search may leave out by name or size (git takes them as pathspecs).
const EXCLUDE_MAX: usize = 2000;
/// Most bytes of those pathspecs.
const EXCLUDE_BYTES: usize = 256 * 1024;

/// Which of the three calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// `project.files`: one directory's entries.
    Files,
    /// `project.read`: one file.
    Read,
    /// `project.search`: lines containing a string.
    Search,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Files => "files",
            Tool::Read => "read",
            Tool::Search => "search",
        }
    }
}

/// The parameters of the three methods.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadParams {
    /// A configured project, or the project of a worker thread.
    pub project: String,
    /// `files`: the directory (default the top); `read`: the file; `search`: only under it.
    pub path: Option<String>,
    /// `search`: the string looked for.
    pub query: Option<String>,
    /// Read the branch of this worker thread of the project instead of the repository's
    /// `HEAD`.
    pub thread: Option<String>,
    /// `read`: the byte to start at.
    pub offset: Option<u64>,
    /// `search`: ignore case.
    pub ignore_case: Option<bool>,
}

/// Where a call reads: the repository's git directory (every command runs there, never
/// finding `.git` in the work tree again) and the commit.
struct At {
    repo: PathBuf,
    /// The repository's top level, resolved: the git directory must stay in it.
    root: PathBuf,
    /// The git directory's device and inode when it was checked ([`git_dir_checked`]): it
    /// must be the same one after the read.
    id: (u64, u64),
    commit: String,
    /// The worker thread whose branch it is (its name).
    branch_of: Option<String>,
}

/// A refusal of a project read (`bad_request`).
fn refuse(msg: impl Into<String>) -> RpcError {
    rpc(codes::BAD_REQUEST, msg)
}

/// A git command of a project read failed: refused without anything git printed (its output
/// can hold the very content the filters would have kept back, such as the matches a killed
/// `git grep` printed before it died), and logged without it too.
fn git_failed(what: &str) -> impl FnOnce(anyhow::Error) -> RpcError + '_ {
    move |_| {
        tracing::warn!("a project read could not {what} (git failed; its output is not kept)");
        refuse(format!(
            "git could not {what} in this repository; narrow the request, or spawn a worker"
        ))
    }
}

impl Hub {
    /// `project.files`, `project.read` and `project.search` (see the module docs). For the
    /// orchestrator, served or refused, one `project.read` event on its thread, in the turn
    /// the call was admitted in.
    pub async fn project_read(
        &self,
        tool: Tool,
        p: ReadParams,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let orch = caller.is_orchestrator();
        let mut rec = ProjectRead {
            tool: tool.name().into(),
            // Redacted whole, then cut: a cut could leave a token too short to recognize.
            project: cut(&redacted(&p.project), RECORD_MAX),
            path: p.path.as_deref().map(|s| cut(&redacted(s), RECORD_MAX)),
            query: p.query.as_deref().map(|s| cut(&redacted(s), RECORD_MAX)),
            branch_of: p.thread.as_deref().map(|s| cut(&redacted(s), RECORD_MAX)),
            ..ProjectRead::default()
        };
        let mut key = None;
        if orch {
            // Past the turn's calls only the first refusal is recorded. (The core lock is
            // let go before recording, which takes it again.)
            let taken = self.inner.core().orch.take_read_call();
            match taken {
                Ok(k) => key = k,
                Err((why, k, first)) => {
                    if first {
                        rec.refused = Some(why.clone());
                        self.record_read(rec, &k)?;
                    }
                    return Err(refuse(why));
                }
            }
        }
        let out = self
            .project_read_checked(tool, &p, key.as_ref(), &mut rec)
            .await
            .map_err(|mut e| {
                // A refusal can name a path from the repository: one holding a token is not
                // repeated (redacted whole, then cut).
                e.message = cut(&redacted(&e.message), REFUSAL_MAX);
                e
            });
        if let Some(key) = &key {
            if let Err(e) = &out {
                rec.refused = Some(e.message.clone());
                rec.bytes = 0;
            }
            self.record_read(rec, key)?;
        }
        out
    }

    async fn project_read_checked(
        &self,
        tool: Tool,
        p: &ReadParams,
        key: Option<&ReadKey>,
        rec: &mut ProjectRead,
    ) -> Result<Value, RpcError> {
        if !self.inner.config.orchestrator.project_reads {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "project reads are off ([orchestrator] project_reads = false in the config); \
                 ask the user, or spawn a worker",
            ));
        }
        if p.project.len() > NAME_MAX || p.thread.as_ref().is_some_and(|t| t.len() > NAME_MAX) {
            return Err(refuse(format!(
                "a project or thread is named in at most {NAME_MAX} bytes"
            )));
        }
        // A token is never looked for, nor read by a name that holds one: neither may confirm
        // a guess, and neither is recorded (`redacted`).
        for s in [
            Some(&p.project),
            p.path.as_ref(),
            p.query.as_ref(),
            p.thread.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(what) = secret_marker(s.as_bytes()) {
                return Err(refuse(format!(
                    "the request holds {what}: twr neither looks for nor reads one"
                )));
            }
        }
        let at = self.read_target(&p.project, p.thread.as_deref()).await?;
        rec.commit = Some(at.commit.clone());
        rec.branch_of = at.branch_of.clone();
        let deny = &self.inner.config.orchestrator.read_deny;
        let (mut result, bytes) = match tool {
            Tool::Files => self.list_files(&at, p, deny).await?,
            Tool::Read => self.read_file(&at, p, deny).await?,
            Tool::Search => self.search(&at, p, deny).await?,
        };
        // The git directory as git read it: one that changed after the check before the read
        // (another directory in its place, a `commondir` or alternates added, a link in its
        // object storage) makes the read fail, whatever it found. (One changed and changed
        // back while git runs is not seen: the security model says so.)
        if git_dir_checked(&p.project, &at.root, &at.repo).await? != at.id {
            return Err(refuse(format!(
                "the git directory of {} changed while it was read: twr reads only \
                 repositories that keep their own objects",
                p.project
            )));
        }
        if let Some(key) = key {
            self.inner
                .core()
                .orch
                .take_read_bytes(key, bytes)
                .map_err(refuse)?;
        }
        rec.bytes = bytes;
        result["project"] = json!(p.project);
        result["commit"] = json!(at.commit);
        result["branch_of"] = json!(at.branch_of);
        Ok(result)
    }

    /// The repository and commit `project` (and `thread`'s branch) names, refused as the
    /// module docs say.
    async fn read_target(&self, project: &str, thread: Option<&str>) -> Result<At, RpcError> {
        // (Its errors never carry git's output: see `Hub::resolve_project_root`.)
        self.resolve_project_root(project).await?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let (root, rev, branch_of) = {
            let core = self.inner.core();
            let target = core.context_target(&self.inner, project, None)?;
            if target.remote {
                return Err(refuse(format!(
                    "{project} is on a remote box: twr reads only the hub's own box's \
                     repositories so far; spawn a worker to read it"
                )));
            }
            let root = target
                .path
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .ok_or_else(|| internal("a context file has no repository"))?;
            // Where the project is as the user gave it: a symbolic link there (a configured
            // repository, or a folder of it in the home folder) could be pointed at another
            // repository, which would then be read under this project's name.
            let given = match self.inner.config.projects.get(project) {
                Some(p) => p.repo.clone(),
                None => root.clone(),
            };
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            if let Some(why) = linked_root(&given, home.as_deref(), &cwd) {
                return Err(refuse(format!("{project}: {why}")));
            }
            let (rev, branch_of) = match thread {
                None => ("HEAD".to_string(), None),
                Some(t) => {
                    let id = core.resolve(t)?;
                    let view = core.view(&id)?;
                    if view.is_orchestrator() || view.project.as_deref() != Some(project) {
                        return Err(refuse(format!(
                            "{t} is not a worker thread of {project}: name one of its threads, \
                             or leave thread out for the repository's HEAD"
                        )));
                    }
                    if view.box_name != LOCAL_BOX {
                        return Err(refuse(format!(
                            "{} works on a remote box: twr reads only the hub's own box's \
                             repositories so far",
                            view.name
                        )));
                    }
                    let rev = match core.rt.get(&id).map(|r| &r.workspace) {
                        Some(git::Workspace::Worktree { branch, .. }) => {
                            format!("refs/heads/{branch}")
                        }
                        // It works in the checkout: its branch is the checkout's.
                        _ => "HEAD".to_string(),
                    };
                    (rev, Some(view.name.clone()))
                }
            };
            (root, rev, branch_of)
        };
        let git = self.local_git()?;
        // Git's top level, symbolic links resolved: every command below runs there, so a
        // link retargeted afterwards changes nothing.
        let repo = git
            .repo_root(root.clone())
            .await
            .map_err(git_failed("find the repository"))?
            .ok_or_else(|| {
                refuse(format!(
                    "{project} ({}) is not a git repository: twr reads only committed files",
                    root.display()
                ))
            })?;
        let protected = protected_dirs(home.as_deref(), &data_dir_of(&self.inner.db_path));
        if let Some(why) = repo_refusal(&repo, home.as_deref(), &protected) {
            return Err(refuse(why));
        }
        // The objects read are the git directory's: it must be the repository's own, not one
        // a `.git` file (or a linked worktree) points to elsewhere, such as a bare repository
        // of the home folder's files.
        let common = git
            .git_common_dir(repo.clone())
            .await
            .map_err(git_failed("find the git directory"))?;
        let git_dir = real(&common);
        let root = real(&repo);
        // Its own git directory, objects and refs only ([`git_dir_checks`]); checked again,
        // all of it, after each read.
        let id = git_dir_checked(project, &root, &git_dir).await?;
        // A missing object of a partial clone would start a fetch, which writes to the
        // repository's config before any transport is refused (on a git that does not know
        // GIT_NO_LAZY_FETCH): refused before any object is read.
        if git
            .partial_clone(git_dir.clone())
            .await
            .map_err(git_failed("read the repository's config"))?
        {
            return Err(refuse(format!(
                "{project} is a partial clone: reading it could start a fetch; spawn a worker \
                 to read it"
            )));
        }
        // From here on every command names that git directory (`--git-dir`), so git discovers
        // nothing: a `.git` the work tree, or the git directory, points elsewhere is never
        // looked at.
        let repo = git_dir;
        let commit = git
            .commit_of(repo.clone(), rev.clone())
            .await
            .map_err(git_failed("find the commit"))?
            .ok_or_else(|| {
                refuse(match &branch_of {
                    Some(t) => format!("{t}'s branch ({rev}) has no commit"),
                    None => format!("{project} has no commit yet"),
                })
            })?;
        Ok(At {
            repo,
            root,
            id,
            commit,
            branch_of,
        })
    }

    /// `project.files`: the entries of directory `path` (default the top).
    async fn list_files(
        &self,
        at: &At,
        p: &ReadParams,
        deny: &[String],
    ) -> Result<(Value, u64), RpcError> {
        let dir = valid_read_path(p.path.as_deref().unwrap_or("")).map_err(refuse)?;
        let git = self.local_git()?;
        let entries = git
            .tree_list(at.repo.clone(), at.commit.clone(), dir.clone())
            .await
            .map_err(git_failed("list the directory"))?
            .ok_or_else(|| {
                rpc(
                    codes::NOT_FOUND,
                    format!("no directory {dir:?} at {}", short(&at.commit)),
                )
            })?;
        let total = entries.len();
        let mut bytes = 0u64;
        let shown: Vec<Value> = entries
            .into_iter()
            .take(LIST_MAX)
            .map(|e| {
                bytes += e.name.len() as u64 + 16;
                let path = if dir.is_empty() {
                    e.name.clone()
                } else {
                    format!("{dir}/{}", e.name)
                };
                // A name that holds a token is never shown.
                let name = redacted(&e.name);
                let mut v = json!({ "name": name, "kind": e.kind, "size": e.size });
                if !e.utf8 {
                    v["utf8"] = json!(false);
                }
                if let Some(why) = secret_name(&path, deny) {
                    v["secret"] = json!(why);
                } else if name != e.name {
                    v["secret"] = json!("its name holds a secret");
                }
                v
            })
            .collect();
        Ok((
            json!({ "path": dir, "entries": shown, "total": total, "more": total > LIST_MAX }),
            bytes,
        ))
    }

    /// `project.read`: file `path`, [`PAGE`] bytes from `offset`.
    async fn read_file(
        &self,
        at: &At,
        p: &ReadParams,
        deny: &[String],
    ) -> Result<(Value, u64), RpcError> {
        let path = valid_read_path(p.path.as_deref().unwrap_or("")).map_err(refuse)?;
        if path.is_empty() {
            return Err(refuse("path is required: the file to read"));
        }
        if let Some(why) = secret_name(&path, deny) {
            return Err(refuse(format!(
                "{path} is {why}: twr never shows it to the orchestrator; do not look for \
                 another way to read it"
            )));
        }
        let git = self.local_git()?;
        let entry = git
            .tree_entry(at.repo.clone(), at.commit.clone(), path.clone())
            .await
            .map_err(git_failed("find the file"))?
            .ok_or_else(|| {
                rpc(
                    codes::NOT_FOUND,
                    format!("no {path} at {}", short(&at.commit)),
                )
            })?;
        match entry.kind {
            EntryKind::Dir => {
                return Err(refuse(format!(
                    "{path} is a directory: list it with list_project_files"
                )))
            }
            EntryKind::Submodule => {
                return Err(refuse(format!(
                    "{path} is a submodule (another repository): twr does not enter it"
                )))
            }
            EntryKind::Symlink => {
                let target = git
                    .blob_read(at.repo.clone(), entry.oid.clone(), LINK_MAX)
                    .await
                    .map_err(git_failed("read the link"))?;
                // A link's target is content too.
                if let Some(what) = secret_marker(&target) {
                    return Err(refuse(format!(
                        "{path} is a link whose target looks like it holds {what}: twr never \
                         shows it to the orchestrator"
                    )));
                }
                let target = String::from_utf8_lossy(&target).into_owned();
                let content = format!("symlink -> {target}");
                let bytes = content.len() as u64;
                return Ok((
                    json!({
                        "path": path, "kind": entry.kind, "content": content,
                        "offset": 0, "end": bytes, "total": bytes,
                        "note": "a symbolic link: twr shows its target and never follows it",
                    }),
                    bytes,
                ));
            }
            EntryKind::File | EntryKind::Executable => {}
        }
        let data = self.text_blob(at, &path, &entry).await?;
        let utf8 = std::str::from_utf8(&data).is_ok();
        let text = String::from_utf8_lossy(&data);
        let offset = usize::try_from(p.offset.unwrap_or(0)).unwrap_or(usize::MAX);
        if offset > text.len() {
            return Err(refuse(format!(
                "offset {offset} is past the end of {path} ({} bytes)",
                text.len()
            )));
        }
        let (start, end) = page(&text, offset, PAGE);
        let content = &text[start..end];
        let mut v = json!({
            "path": path, "kind": entry.kind, "content": content,
            "offset": start, "end": end, "total": text.len(),
        });
        if !utf8 {
            v["utf8"] = json!(false);
        }
        Ok((v, content.len() as u64))
    }

    /// The bytes of file `entry` (at `path`), refused when it is over [`READ_MAX`], binary,
    /// or holds a secret ([`secret_marker`]): the whole file, whatever part is asked for.
    async fn text_blob(&self, at: &At, path: &str, entry: &TreeEntry) -> Result<Vec<u8>, RpcError> {
        if let Some(size) = entry.size.filter(|s| *s > READ_MAX) {
            return Err(refuse(format!(
                "{path} is {size} bytes, over the {} KB twr reads",
                READ_MAX / 1024
            )));
        }
        let data = self
            .local_git()?
            .blob_read(at.repo.clone(), entry.oid.clone(), READ_MAX)
            .await
            .map_err(git_failed("read the file"))?;
        if data[..data.len().min(BINARY_PROBE)].contains(&0) {
            return Err(refuse(format!(
                "{path} is a binary file: twr reads text only"
            )));
        }
        if let Some(what) = secret_marker(&data) {
            return Err(refuse(format!(
                "{path} looks like it holds {what}: twr never shows it to the orchestrator; \
                 do not look for another way to read it"
            )));
        }
        Ok(data)
    }

    /// `project.search`: lines with `query` (a fixed string), [`SEARCH_PER_FILE`] per file,
    /// [`SEARCH_HITS`] in all. Files a read would refuse by name or size never reach git grep
    /// (it does not even load them), and a file's lines come back only once the whole file
    /// passed the checks a read makes ([`Hub::text_blob`]): a line of a key file need not look
    /// like one. A file that fails them is left out of the search, and git searches again
    /// ([`SEARCH_ROUNDS`] times at most), so nothing in the answer, its lines or `more`, says
    /// whether a file it leaves out matched. A file whose name is not UTF-8 (which no
    /// pathspec can leave out) is read first: one a read would refuse (a secret, binary)
    /// refuses the search, whatever it looks for. Its lines are never shown, so git returns
    /// [`SEARCH_PER_FILE`] more lines for each such file, and `more` comes only from a line
    /// checked and shown. The one answer that depends on refused
    /// files' content: more of them matching than those rounds leave out refuses the search
    /// too.
    async fn search(
        &self,
        at: &At,
        p: &ReadParams,
        deny: &[String],
    ) -> Result<(Value, u64), RpcError> {
        let query = p.query.as_deref().unwrap_or("");
        if query.trim().is_empty() || query.len() > QUERY_MAX || query.chars().any(char::is_control)
        {
            return Err(refuse(format!(
                "query is one line of 1 to {QUERY_MAX} bytes, without control characters"
            )));
        }
        let dir = match p.path.as_deref() {
            Some(d) => Some(valid_read_path(d).map_err(refuse)?).filter(|d| !d.is_empty()),
            None => None,
        };
        let git = self.local_git()?;
        let files = git
            .tree_files(
                at.repo.clone(),
                at.commit.clone(),
                dir.clone().unwrap_or_default(),
            )
            .await
            .map_err(git_failed("list the files to search (narrow it with path)"))?;
        let mut exclude = Vec::new();
        let mut exclude_bytes = 0usize;
        // Files whose names are not UTF-8, which only their content can take out.
        let mut unnamed = Vec::new();
        // A crafted tree can hold one path twice: git grep reads both, a check only one.
        // (Names that are not UTF-8 are compared by their bytes' replaced spelling, which two
        // different ones can share: never shown, they need no check.)
        let mut seen = std::collections::HashSet::new();
        for f in &files {
            if f.utf8 && !seen.insert(f.name.as_str()) {
                return Err(refuse(
                    "the commit holds one path twice: narrow the search with path",
                ));
            }
            if !matches!(f.kind, EntryKind::File | EntryKind::Executable) {
                continue;
            }
            let big = f.size.is_some_and(|s| s > READ_MAX);
            let never = big
                || secret_name(&f.name, deny).is_some()
                || secret_marker(f.name.as_bytes()).is_some();
            if !f.utf8 {
                // Not nameable in a pathspec: git would search it, and whether it matched
                // could show in `more`. So it is either searched with nothing to hide (its
                // lines are never shown, its name could be another's), or the search is
                // refused whatever it looks for.
                if never {
                    return Err(refuse(
                        "a file here that twr never searches has a name that is not UTF-8: \
                         narrow the search with path",
                    ));
                }
                unnamed.push(f);
                continue;
            }
            if never {
                exclude_bytes += f.name.len() + 20;
                exclude.push(f.name.clone());
            }
        }
        if unnamed.len() > UNNAMED_MAX {
            return Err(refuse(
                "too many files here have names that are not UTF-8: narrow the search with path",
            ));
        }
        // Their lines are never shown, so git returns that many more (each file gives at most
        // `SEARCH_PER_FILE`): a window they fill would leave `more` to git, which says whether
        // a line past it matched, one never checked (a refused file's).
        let window = SEARCH_CANDIDATES + unnamed.len() * SEARCH_PER_FILE as usize;
        // Each must be one a read would serve whole (no secret, not binary: git searches a
        // binary file whose attributes say it is text).
        for f in unnamed {
            if self.text_blob(at, &f.name, f).await.is_err() {
                return Err(refuse(
                    "a file here that twr never searches has a name that is not UTF-8: \
                     narrow the search with path",
                ));
            }
        }
        // Each named file's verdict, checked once: whether a read would serve it whole.
        let mut checked: std::collections::HashMap<String, bool> = Default::default();
        // Files whose content a read refuses are found only by searching: each one found
        // among the lines git returns is left out and git searches again, until none is.
        // The lines kept, and whether there were more, are then those of files a read
        // serves (or never names) alone, as if the refused ones had never been there.
        let mut rounds = 0;
        let found = loop {
            if exclude.len() > EXCLUDE_MAX || exclude_bytes > EXCLUDE_BYTES {
                return Err(refuse(
                    "too many files here that twr never searches: narrow the search with path",
                ));
            }
            let found = git
                .grep_commit(
                    at.repo.clone(),
                    GrepRequest {
                        commit: at.commit.clone(),
                        query: query.to_string(),
                        dir: dir.clone(),
                        ignore_case: p.ignore_case.unwrap_or(false),
                        per_file: SEARCH_PER_FILE,
                        // More than are shown: whether there are more is known from the
                        // lines of files that passed.
                        max_hits: window,
                        exclude: exclude.clone(),
                    },
                )
                .await
                .map_err(git_failed("search (too many or too long matching lines?)"))?;
            let mut refused = false;
            for h in &found.hits {
                // A name that is not UTF-8 is never shown: its replaced spelling could be
                // another file's, whose verdict is cached under it. (Checked above.)
                if !h.utf8 {
                    continue;
                }
                let ok = match checked.get(&h.path) {
                    // Left out already: git found it again (never expected).
                    Some(false) => false,
                    Some(true) => true,
                    None => {
                        let ok = self.searchable(at, &h.path, h.utf8, deny).await;
                        checked.insert(h.path.clone(), ok);
                        if !ok {
                            exclude_bytes += h.path.len() + 20;
                            exclude.push(h.path.clone());
                        }
                        ok
                    }
                };
                refused |= !ok;
            }
            if !refused {
                break found;
            }
            rounds += 1;
            if rounds == SEARCH_ROUNDS {
                return Err(refuse(
                    "too many files here that twr never searches: narrow the search with path",
                ));
            }
        };
        let mut hits = Vec::new();
        let mut more = false;
        let mut bytes = 0u64;
        for h in found.hits {
            if !h.utf8 || secret_marker(h.text.as_bytes()).is_some() {
                continue;
            }
            if hits.len() == SEARCH_HITS {
                more = true;
                break;
            }
            let text = cut(h.text.trim_end(), SEARCH_LINE_MAX);
            bytes += (h.path.len() + text.len()) as u64;
            hits.push(json!({ "path": h.path, "line": h.line, "text": text }));
        }
        // `more` only ever comes from a line checked and shown: git's says whether a line past
        // the window matched, which nothing checked. The window leaves more than
        // `SEARCH_HITS` lines that can be shown whenever git cut it, so this never refuses.
        if found.more && !more {
            return Err(refuse(
                "too many lines here that twr never shows: narrow the search with path",
            ));
        }
        Ok((
            json!({ "query": query, "path": dir, "hits": hits, "more": more }),
            bytes,
        ))
    }

    /// Whether search may show lines of the file at `path`: a name that is no secret and
    /// holds none, valid UTF-8 (it is found again by name), a text file a read would serve.
    async fn searchable(&self, at: &At, path: &str, utf8: bool, deny: &[String]) -> bool {
        if !utf8
            || secret_name(path, deny).is_some()
            || secret_marker(path.as_bytes()).is_some()
            || valid_read_path(path).is_err()
        {
            return false;
        }
        let Ok(git) = self.local_git() else {
            return false;
        };
        let entry = match git
            .tree_entry(at.repo.clone(), at.commit.clone(), path.to_string())
            .await
        {
            Ok(Some(e)) if matches!(e.kind, EntryKind::File | EntryKind::Executable) => e,
            _ => return false,
        };
        self.text_blob(at, path, &entry).await.is_ok()
    }

    fn local_git(&self) -> Result<std::sync::Arc<dyn twr_core::git::GitOps>, RpcError> {
        Ok(self
            .inner
            .hosts
            .get(LOCAL_BOX)
            .ok_or_else(|| internal("the hub has no local box"))?
            .git())
    }

    /// Appends `rec` to the orchestrator thread and turn of `key` (the call's admission).
    fn record_read(&self, rec: ProjectRead, key: &ReadKey) -> Result<(), RpcError> {
        self.inner
            .core()
            .append(
                &self.inner,
                &key.thread,
                key.turn,
                Source::Hub,
                EventBody::ProjectRead(rec),
            )
            .map_err(internal)?;
        Ok(())
    }
}

/// `s`, or a placeholder when it holds a secret ([`secret_marker`]): what an event records and
/// a listing shows of a name.
fn redacted(s: &str) -> String {
    match secret_marker(s.as_bytes()) {
        Some(what) => format!("[redacted: holds {what}]"),
        None => s.to_string(),
    }
}

/// Why repository path `given` (as configured, or as a thread recorded it; a relative one is
/// taken from `cwd`, as git takes it) may be redirected: it has a `..` (whose meaning depends
/// on links), it is a symbolic link, or a folder of it inside the home folder `home` is.
/// "Inside" is decided on resolved paths, one part at a time, however either is spelled:
/// links above the home folder (a system's `/home` -> `/usr/home`) are followed, any link in
/// it is refused.
pub fn linked_root(given: &Path, home: Option<&Path>, cwd: &Path) -> Option<String> {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::path::Component;
    if given.components().any(|c| c == Component::ParentDir) {
        return Some(format!(
            "{} has a `..`: configure the repository's absolute path without one",
            given.display()
        ));
    }
    let refused = |p: &Path| {
        Some(format!(
            "{} is a symbolic link, or leads through one in the home folder: configure the \
             repository's real path",
            p.display()
        ))
    };
    let home = home.map(real);
    // The parts of `p` still to walk: `None` is a `..` a link's target holds.
    fn parts(p: &Path) -> Vec<Option<OsString>> {
        p.components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(Some(n.to_os_string())),
                Component::ParentDir => Some(None),
                _ => None,
            })
            .collect()
    }
    let given: PathBuf = cwd.join(given);
    let mut todo: VecDeque<Option<OsString>> = parts(&given).into();
    // Where the parts so far lead, every link on the way taken apart part by part, so a
    // link inside a followed link's target is checked like one in the path.
    let mut at = PathBuf::from("/");
    let mut hops = 0;
    while let Some(part) = todo.pop_front() {
        let Some(name) = part else {
            at.pop();
            continue;
        };
        let next = at.join(&name);
        let Ok(target) = std::fs::read_link(&next) else {
            at = next;
            continue;
        };
        let leaf = todo.is_empty();
        let in_home = home.as_deref().is_some_and(|h| at.starts_with(h));
        hops += 1;
        if leaf || in_home || hops > MAX_HOPS {
            return refused(&given);
        }
        if target.is_absolute() {
            at = PathBuf::from("/");
        }
        for (i, p) in parts(&target).into_iter().enumerate() {
            todo.insert(i, p);
        }
    }
    None
}

/// Most links [`linked_root`] follows in one path (more is a loop, refused).
const MAX_HOPS: usize = 40;

/// `p` with symbolic links resolved, as it is when it cannot be.
fn real(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Most entries [`storage_redirect`] looks at in one object storage (loose objects pile up
/// where nothing packs them); past it the repository is refused.
const STORAGE_ENTRIES_MAX: usize = 1_000_000;

/// [`git_dir_checks`] of project `project`'s git directory `git_dir` (resolved) in its
/// repository's top level `root` (resolved), off the async threads: its device and inode, or
/// the refusal.
async fn git_dir_checked(
    project: &str,
    root: &Path,
    git_dir: &Path,
) -> Result<(u64, u64), RpcError> {
    let (root, dir) = (root.to_path_buf(), git_dir.to_path_buf());
    let checked = tokio::task::spawn_blocking(move || git_dir_checks(&root, &dir))
        .await
        .map_err(internal)?;
    checked.map_err(|why| {
        refuse(format!(
            "the git directory of {project} {why}: twr reads only repositories that keep \
             their own objects"
        ))
    })
}

/// Why the git directory `git_dir` (as resolved when it was found) may not be the
/// repository's own, with its own objects: it no longer resolves to itself (a link took its
/// place), it is outside the repository's top level `root`, it has a `commondir` (another git
/// directory's refs and objects) or alternates (another's objects; `http-alternates` too),
/// or its object storage has a link in it ([`storage_redirect`]). Otherwise its device and
/// inode.
pub fn git_dir_checks(root: &Path, git_dir: &Path) -> Result<(u64, u64), String> {
    use std::os::unix::fs::MetadataExt;
    if real(git_dir) != git_dir {
        return Err(format!(
            "({}) is a symbolic link, or leads through one",
            git_dir.display()
        ));
    }
    if !git_dir.starts_with(real(root)) {
        return Err(format!(
            "({}) is outside its repository ({})",
            git_dir.display(),
            root.display()
        ));
    }
    for redirect in [
        "commondir",
        "objects/info/alternates",
        "objects/info/http-alternates",
    ] {
        if std::fs::symlink_metadata(git_dir.join(redirect)).is_ok() {
            return Err(format!("has {redirect}, which takes git elsewhere"));
        }
    }
    if let Some(why) = storage_redirect(git_dir) {
        return Err(why);
    }
    let m = std::fs::symlink_metadata(git_dir).map_err(|e| format!("cannot be read ({e})"))?;
    Ok((m.dev(), m.ino()))
}

/// Why the object storage of git directory `git_dir` (resolved) may hold another
/// repository's objects: its `objects` folder, or anything in it at any depth (a loose
/// object's folder or file, a pack, an index, `info`), is a symbolic link, or is neither a
/// folder nor a regular file (a pipe git would wait on). Every entry is looked at as it is,
/// never through a link, so with none found every object git reads there is the git
/// directory's own. A missing `objects` folder holds nothing to read; one entry that goes
/// while this walks (git packing) is skipped. More than [`STORAGE_ENTRIES_MAX`] entries, or
/// a folder that cannot be listed, is refused too.
pub fn storage_redirect(git_dir: &Path) -> Option<String> {
    use std::io::ErrorKind::NotFound;
    let objects = git_dir.join("objects");
    match std::fs::symlink_metadata(&objects) {
        Err(e) if e.kind() == NotFound => return None,
        Err(e) => return Some(format!("has objects that cannot be read ({e})")),
        Ok(m) if m.file_type().is_symlink() => {
            return Some("has objects that are a symbolic link, which takes git elsewhere".into())
        }
        Ok(m) if !m.is_dir() => return Some("has objects that are not a folder".into()),
        Ok(_) => {}
    }
    let mut todo = vec![objects.clone()];
    let mut seen = 0usize;
    while let Some(dir) = todo.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == NotFound && dir != objects => continue,
            Err(e) => return Some(format!("has an object folder that cannot be listed ({e})")),
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) if e.kind() == NotFound => continue,
                Err(e) => return Some(format!("has an object folder that cannot be listed ({e})")),
            };
            seen += 1;
            if seen > STORAGE_ENTRIES_MAX {
                return Some(format!(
                    "has more than {STORAGE_ENTRIES_MAX} entries in its object storage"
                ));
            }
            let path = entry.path();
            let rel = path.strip_prefix(git_dir).unwrap_or(&path).display();
            // The entry's own type (from the listing, or `lstat`): a link is never followed.
            let kind = match entry.file_type() {
                Ok(k) => k,
                Err(e) if e.kind() == NotFound => continue,
                Err(e) => return Some(format!("has {rel}, which cannot be read ({e})")),
            };
            if kind.is_symlink() {
                return Some(format!(
                    "has {rel}, a symbolic link in its object storage, which takes git elsewhere"
                ));
            } else if kind.is_dir() {
                todo.push(path);
            } else if !kind.is_file() {
                return Some(format!(
                    "has {rel} in its object storage, which is not a regular file"
                ));
            }
        }
    }
    None
}

/// The folders no repository the orchestrator reads may hold: twr's data directory and
/// `TWR_HOME`, and the home folder's credential and agent folders.
pub fn protected_dirs(home: Option<&Path>, data_dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![data_dir.to_path_buf()];
    if let Some(h) = std::env::var_os("TWR_HOME").filter(|h| !h.is_empty()) {
        out.push(PathBuf::from(h));
    }
    if let Some(home) = home {
        for d in PRIVATE_DIR_NAMES
            .iter()
            .chain(&[".netrc", ".git-credentials", ".local/share/twr"])
        {
            out.push(home.join(d));
        }
    }
    out
}

/// Why the orchestrator may not read repository `repo` (resolved): it is, or holds, the home
/// folder `home`, or it holds or is in one of `protected`.
pub fn repo_refusal(repo: &Path, home: Option<&Path>, protected: &[PathBuf]) -> Option<String> {
    let repo = real(repo);
    if let Some(home) = home {
        if real(home).starts_with(&repo) {
            return Some(format!(
                "the repository {} holds the home folder: twr does not read it",
                repo.display()
            ));
        }
    }
    protected.iter().find_map(|p| {
        let p_real = real(p);
        if p_real.starts_with(&repo) {
            Some(format!(
                "the repository {} holds {}: twr does not read it",
                repo.display(),
                p.display()
            ))
        } else if repo.starts_with(&p_real) {
            Some(format!(
                "the repository {} is in {}: twr does not read it",
                repo.display(),
                p.display()
            ))
        } else {
            None
        }
    })
}

/// Why a thread in repository `repo` (resolved, on another box, whose home folder twr does not
/// know) may not be started by the orchestrator, judged by the path's shape: the root folder, a
/// home folder (`/root`, `/home/<user>`, `/Users/<user>`), or a path through a credentials or
/// agent folder (`.ssh`, `.config`, `.claude`, `.local/share/twr`, ...: the names
/// [`protected_dirs`] lists under a home folder).
pub fn remote_repo_refusal(repo: &Path) -> Option<String> {
    use std::path::Component;
    let parts: Vec<String> = repo
        .components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let home_shaped = match parts.as_slice() {
        [] => true,
        [one] => one == "root" || one == "home" || one == "Users",
        [top, _] => top == "home" || top == "Users",
        _ => false,
    };
    if home_shaped {
        return Some(format!(
            "the repository {} is the root or a home folder",
            repo.display()
        ));
    }
    let private = parts.iter().enumerate().any(|(i, p)| {
        PRIVATE_DIR_NAMES.contains(&p.as_str())
            || (p == ".local"
                && parts.get(i + 1).map(String::as_str) == Some("share")
                && parts.get(i + 2).map(String::as_str) == Some("twr"))
    });
    private.then(|| {
        format!(
            "the repository {} is in a credentials or agent folder",
            repo.display()
        )
    })
}

/// The folder names of [`protected_dirs`] under a home folder (`.local/share/twr` aside).
const PRIVATE_DIR_NAMES: &[&str] = &[
    ".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker", ".config", ".claude", ".codex", ".grok",
];

/// Folder names whose files are credentials wherever they are.
const SECRET_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker"];

/// File extensions of keys, keystores and infrastructure state.
const SECRET_EXTS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "ppk", "tfstate", "tfvars", "ovpn",
];

/// File names that are credentials.
const SECRET_NAMES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".envrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".htpasswd",
    ".dockercfg",
    ".pgpass",
    ".s3cfg",
    "credentials",
    "kubeconfig",
];

/// Extensions of configuration and data files, where a name with `secret` or `credential` in
/// it means one (not source code about secrets).
const CONFIG_EXTS: &[&str] = &[
    "",
    "json",
    "yml",
    "yaml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "env",
    "txt",
    "xml",
    "properties",
    "plist",
    "csv",
];

/// Why the file at `path` (from the repository's top) looks like a secret by its name or a
/// folder it is in, or matches a `deny` glob (`[orchestrator] read_deny`); `None` otherwise.
pub fn secret_name(path: &str, deny: &[String]) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('/').collect();
    let name = parts.last().copied().unwrap_or_default();
    if let Some(d) = parts[..parts.len().saturating_sub(1)]
        .iter()
        .find(|p| SECRET_DIRS.contains(p))
    {
        return Some(format!("in a {d} folder, which holds credentials"));
    }
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    let template = [".example", ".sample", ".template", ".dist"]
        .iter()
        .any(|s| name.ends_with(s));
    if (name == ".env" || name.starts_with(".env.") || name.ends_with(".env")) && !template {
        return Some("an environment file, which holds secrets".into());
    }
    if ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
        .iter()
        .any(|k| name.starts_with(k))
        && !name.ends_with(".pub")
    {
        return Some("a private SSH key".into());
    }
    if SECRET_EXTS.contains(&ext) || name.contains(".tfstate") {
        return Some(format!(
            "a .{ext} file (keys, keystores and state hold secrets)"
        ));
    }
    if SECRET_NAMES.contains(&name) || name.starts_with("credentials.") {
        return Some("a credentials file".into());
    }
    if name.starts_with("service-account") && ext == "json" {
        return Some("a service account key".into());
    }
    if (name.contains("secret") || name.contains("credential"))
        && CONFIG_EXTS.contains(&ext)
        && !template
    {
        return Some("a secrets file".into());
    }
    deny.iter()
        .find(|g| {
            let g = g.to_ascii_lowercase();
            if g.contains('/') {
                glob_match(g.trim_start_matches('/'), &lower)
            } else {
                glob_match(&g, name)
            }
        })
        .map(|g| format!("denied by [orchestrator] read_deny {g:?}"))
}

/// Whether `text` matches glob `pat`: `*` is any run within a path part, `**` any run across
/// parts (`**/` also none), `?` one character but `/`. Dynamic programming over the two, so
/// a pattern of many stars costs its length times the text's, never more.
pub fn glob_match(pat: &str, text: &str) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum Tok {
        Char(char),
        Any,
        Star,
        Stars,
        StarsSlash,
    }
    let pc: Vec<char> = pat.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < pc.len() {
        match pc[i] {
            '*' if pc.get(i + 1) == Some(&'*') => {
                if pc.get(i + 2) == Some(&'/') {
                    toks.push(Tok::StarsSlash);
                    i += 3;
                } else {
                    toks.push(Tok::Stars);
                    i += 2;
                }
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '?' => {
                toks.push(Tok::Any);
                i += 1;
            }
            c => {
                toks.push(Tok::Char(c));
                i += 1;
            }
        }
    }
    let t: Vec<char> = text.chars().collect();
    // ok[j]: the tokens so far match t[..j].
    let mut ok = vec![false; t.len() + 1];
    ok[0] = true;
    for tok in toks {
        let mut next = vec![false; t.len() + 1];
        match tok {
            Tok::Char(c) => {
                for j in 0..t.len() {
                    next[j + 1] = ok[j] && t[j] == c;
                }
            }
            Tok::Any => {
                for j in 0..t.len() {
                    next[j + 1] = ok[j] && t[j] != '/';
                }
            }
            Tok::Star => {
                for j in 0..=t.len() {
                    next[j] = ok[j] || (j > 0 && next[j - 1] && t[j - 1] != '/');
                }
            }
            Tok::Stars => {
                for j in 0..=t.len() {
                    next[j] = ok[j] || (j > 0 && next[j - 1]);
                }
            }
            Tok::StarsSlash => {
                // Nothing, or any run ending with `/`.
                let mut seen = false;
                for j in 0..=t.len() {
                    seen |= ok[j];
                    next[j] = ok[j] || (seen && j > 0 && t[j - 1] == '/');
                }
            }
        }
        ok = next;
    }
    ok[t.len()]
}

/// A token format: `(prefix, least characters after it, what it is, which characters)`.
type TokenFormat = (&'static str, usize, &'static str, fn(u8) -> bool);

/// The token formats [`secret_marker`] finds.
const TOKENS: &[TokenFormat] = &[
    ("AKIA", 16, "an AWS access key", upper_alnum),
    ("ASIA", 16, "an AWS access key", upper_alnum),
    ("ghp_", 36, "a GitHub token", alnum),
    ("gho_", 36, "a GitHub token", alnum),
    ("ghu_", 36, "a GitHub token", alnum),
    ("ghs_", 36, "a GitHub token", alnum),
    ("ghr_", 36, "a GitHub token", alnum),
    ("github_pat_", 22, "a GitHub token", word),
    ("glpat-", 20, "a GitLab token", token),
    ("xoxa-", 10, "a Slack token", token),
    ("xoxb-", 10, "a Slack token", token),
    ("xoxp-", 10, "a Slack token", token),
    ("xoxr-", 10, "a Slack token", token),
    ("xoxs-", 10, "a Slack token", token),
    ("sk-ant-", 20, "an Anthropic API key", token),
    ("sk-proj-", 20, "an OpenAI API key", token),
    ("sk_live_", 16, "a Stripe key", alnum),
    ("rk_live_", 16, "a Stripe key", alnum),
    ("AIza", 35, "a Google API key", token),
    ("npm_", 36, "an npm token", alnum),
];

fn upper_alnum(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}
fn alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}
fn word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}
fn token(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// What secret `data` looks like it holds: a PEM private key, or a token of a known format
/// standing alone (not inside a longer word); `None` otherwise.
pub fn secret_marker(data: &[u8]) -> Option<&'static str> {
    const BEGIN: &[u8] = b"-----BEGIN ";
    for i in 0..data.len() {
        let rest = &data[i..];
        if rest.starts_with(BEGIN) {
            // The rest of the line, at most 100 bytes of it (never a search to the end).
            let window = &rest[..rest.len().min(100)];
            let line = &window[..window
                .iter()
                .position(|c| *c == b'\n')
                .unwrap_or(window.len())];
            if line.windows(11).any(|w| w == b"PRIVATE KEY") {
                return Some("a private key");
            }
        }
        if i > 0 && token(data[i - 1]) {
            continue;
        }
        for (prefix, least, what, ok) in TOKENS {
            if rest.starts_with(prefix.as_bytes())
                && rest[prefix.len()..].iter().take_while(|c| ok(**c)).count() >= *least
            {
                return Some(what);
            }
        }
    }
    None
}

/// The page of `text` from `offset` (moved to a character boundary): at most `max` bytes,
/// ending at a line end when one is in the second half.
fn page(text: &str, offset: usize, max: usize) -> (usize, usize) {
    let mut start = offset.min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (start + max).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end < text.len() {
        if let Some(nl) = text[start..end].rfind('\n') {
            if nl + 1 > max / 2 {
                end = start + nl + 1;
            }
        }
    }
    (start, end)
}

/// `s` cut to at most `max` bytes on a character boundary.
fn cut(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// A commit's short id.
fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_storage_with_a_link_or_a_pipe_in_it_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join("repo/.git");
        // No objects folder: nothing to read.
        std::fs::create_dir_all(&git_dir).unwrap();
        assert_eq!(storage_redirect(&git_dir), None);
        let objects = git_dir.join("objects");
        for d in ["ab", "pack", "info/commit-graphs"] {
            std::fs::create_dir_all(objects.join(d)).unwrap();
        }
        std::fs::write(objects.join("ab/cdef"), b"x").unwrap();
        std::fs::write(objects.join("pack/pack-1.pack"), b"x").unwrap();
        assert_eq!(storage_redirect(&git_dir), None);
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        // A link at any depth, whether or not its target exists.
        for at in [
            "info/commit-graphs/graph-1.graph",
            "pack/pack-2.idx",
            "cd",
            "ab/0123",
        ] {
            let link = objects.join(at);
            std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
            let why = storage_redirect(&git_dir).unwrap();
            assert!(why.contains("symbolic link") && why.contains(at), "{why}");
            std::fs::remove_file(&link).unwrap();
        }
        assert_eq!(storage_redirect(&git_dir), None);
        let fifo = objects.join("pack/pack-3.pack");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        assert!(storage_redirect(&git_dir)
            .unwrap()
            .contains("not a regular file"));
        std::fs::remove_file(&fifo).unwrap();
        // The objects folder itself a link (to a real one).
        let real_objects = dir.path().join("real-objects");
        std::fs::rename(&objects, &real_objects).unwrap();
        std::os::unix::fs::symlink(&real_objects, &objects).unwrap();
        assert!(storage_redirect(&git_dir)
            .unwrap()
            .contains("symbolic link"));
    }

    #[test]
    fn secret_names_are_refused_and_their_templates_are_not() {
        for p in [
            ".env",
            "app/.env.local",
            "prod.env",
            ".env.production",
            "deploy/id_ed25519",
            "certs/server.pem",
            "tls.key",
            "store.p12",
            "infra/terraform.tfstate",
            "infra/terraform.tfstate.backup",
            "prod.tfvars",
            ".npmrc",
            ".git-credentials",
            "credentials",
            "credentials.json",
            "config/secrets.yml",
            "client_secret.json",
            "service-account-prod.json",
            "home/.ssh/config",
            ".aws/config",
            ".kube/config",
            "kubeconfig",
            "x/.docker/config.json",
            "ENV/.ENV",
            ".envrc",
        ] {
            assert!(secret_name(p, &[]).is_some(), "{p}");
        }
        for p in [
            ".env.example",
            ".env.sample",
            "config/.env.template",
            "id_ed25519.pub",
            "README.md",
            "src/secrets.rs",
            "src/credential_store.go",
            "docs/secrets.md",
            "environment.rs",
            "keys.rs",
            ".envrc.md",
        ] {
            assert_eq!(secret_name(p, &[]), None, "{p}");
        }
        let deny = vec!["*.sqlite".to_string(), "config/prod/**".to_string()];
        assert!(secret_name("data/app.sqlite", &deny).is_some());
        assert!(secret_name("config/prod/db.yml", &deny).is_some());
        assert!(secret_name("config/prod/a/b.rb", &deny).is_some());
        assert_eq!(secret_name("config/dev/db.yml", &deny), None);
    }

    #[test]
    fn many_star_globs_stay_cheap() {
        let start = std::time::Instant::now();
        assert!(!glob_match("*a*a*a*a*a*a*a*a*a*a*b", &"a".repeat(200)));
        assert!(glob_match(
            "*a*a*a*a*a*a*a*a*a*a*b",
            &("a".repeat(200) + "b")
        ));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn links_to_repositories_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("code/private")).unwrap();
        std::fs::create_dir_all(home.join("real/code/x")).unwrap();
        std::os::unix::fs::symlink(home.join("code/private"), home.join("code/api")).unwrap();
        std::os::unix::fs::symlink(home.join("real/code"), home.join("linked")).unwrap();
        let root = Path::new("/");
        let check = |p: &Path| linked_root(p, Some(&home), root);
        assert_eq!(check(&home.join("code/private")), None);
        assert!(check(&home.join("code/api")).is_some());
        assert!(check(&home.join("linked/x")).is_some());
        let slash = PathBuf::from(format!("{}/", home.join("code/api").display()));
        assert!(check(&slash).is_some(), "a trailing / changes nothing");
        // A relative path is taken from the working directory, as git takes it.
        assert!(linked_root(Path::new("linked/x"), Some(&home), &home).is_some());
        assert!(linked_root(Path::new("./code/api"), Some(&home), &home).is_some());
        assert_eq!(
            linked_root(Path::new("code/private"), Some(&home), &home),
            None
        );
        // A `..` could step through a link whatever the spelling.
        let dotdot = dir.path().join("x/../home/linked/x");
        assert!(check(&dotdot).unwrap().contains(".."));
        // A link above the home folder is the system's, not a project's.
        let above = dir.path().join("homelink");
        std::os::unix::fs::symlink(&home, &above).unwrap();
        assert_eq!(
            linked_root(&above.join("code/private"), Some(&above), root),
            None
        );
        // A home folder spelled through that link, a working directory spelled resolved.
        assert!(linked_root(Path::new("linked/x"), Some(&above), &home).is_some());
        assert!(linked_root(&home.join("linked/x"), Some(&above), root).is_some());
        // And the other way round: the path through the system link, HOME resolved.
        assert!(linked_root(&above.join("linked/x"), Some(&home), root).is_some());
        assert_eq!(
            linked_root(&above.join("real/code/x"), Some(&home), root),
            None
        );
        // A link outside the home folder whose target goes through a link inside it.
        let opt = dir.path().join("opt");
        std::fs::create_dir_all(&opt).unwrap();
        std::os::unix::fs::symlink(home.join("linked"), opt.join("code")).unwrap();
        assert!(linked_root(&opt.join("code/x"), Some(&home), root).is_some());
        std::os::unix::fs::symlink(home.join("real/code"), opt.join("plain")).unwrap();
        assert_eq!(linked_root(&opt.join("plain/x"), Some(&home), root), None);
        // A loop is refused, not walked forever.
        std::os::unix::fs::symlink(opt.join("loop"), opt.join("loop")).unwrap();
        assert!(linked_root(&opt.join("loop/x"), Some(&home), root).is_some());
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.sqlite", "app.sqlite"));
        assert!(!glob_match("*.sqlite", "a/app.sqlite"));
        assert!(glob_match("**/*.sqlite", "a/b/app.sqlite"));
        assert!(glob_match("**/*.sqlite", "app.sqlite"));
        assert!(glob_match("config/**", "config/a/b"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
        assert!(glob_match("config/**/db.yml", "config/db.yml"));
        assert!(glob_match("config/**/db.yml", "config/a/b/db.yml"));
        assert!(!glob_match("config/**/db.yml", "config/adb.yml"));
    }

    #[test]
    fn markers_find_keys_and_tokens_standing_alone() {
        let key = b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n";
        assert_eq!(secret_marker(key), Some("a private key"));
        assert_eq!(
            secret_marker(b"x\n-----BEGIN RSA PRIVATE KEY-----\n"),
            Some("a private key")
        );
        assert_eq!(secret_marker(b"-----BEGIN CERTIFICATE-----\n"), None);
        assert_eq!(
            secret_marker(b"aws_key = AKIAIOSFODNN7EXAMPLE\n"),
            Some("an AWS access key")
        );
        let gh = format!("token: ghp_{}", "a".repeat(36));
        assert_eq!(secret_marker(gh.as_bytes()), Some("a GitHub token"));
        let ant = format!("KEY=sk-ant-api03-{}", "x".repeat(30));
        assert_eq!(secret_marker(ant.as_bytes()), Some("an Anthropic API key"));
        // Prose and identifiers that only start like one.
        assert_eq!(secret_marker(b"the AKIA prefix marks AWS keys"), None);
        assert_eq!(secret_marker(b"fn ghp_parse() {}"), None);
        assert_eq!(
            secret_marker(format!("MYAKIA{}", "A".repeat(16)).as_bytes()),
            None
        );
    }

    #[test]
    fn remote_repositories_are_judged_by_their_shape() {
        for refused in [
            "/",
            "/root",
            "/home",
            "/home/me",
            "/Users/me",
            "/home/me/.ssh/x",
            "/home/me/.config/nvim",
            "/srv/u/.claude/projects",
            "/home/me/.local/share/twr/worktrees/api/w1",
        ] {
            assert!(
                remote_repo_refusal(Path::new(refused)).is_some(),
                "{refused}"
            );
        }
        for fine in [
            "/home/me/code/api",
            "/srv/repos/api",
            "/Users/me/src/web",
            "/home/me/.local/share/other",
            "/opt/configs",
        ] {
            assert_eq!(remote_repo_refusal(Path::new(fine)), None, "{fine}");
        }
    }

    #[test]
    fn repositories_that_hold_protected_folders_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let data = home.join(".local/share/twr");
        std::fs::create_dir_all(home.join("code/api")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let protected = protected_dirs(Some(&home), &data);
        assert_eq!(
            repo_refusal(&home.join("code/api"), Some(&home), &protected),
            None
        );
        assert!(repo_refusal(&home, Some(&home), &protected)
            .unwrap()
            .contains("home folder"));
        assert!(repo_refusal(dir.path(), Some(&home), &protected).is_some());
        assert!(repo_refusal(&home.join(".local"), Some(&home), &protected)
            .unwrap()
            .contains("twr"));
        // Nor one inside such a folder.
        std::fs::create_dir_all(home.join(".codex/snapshots")).unwrap();
        std::fs::create_dir_all(data.join("worktrees/api/w1")).unwrap();
        assert!(
            repo_refusal(&home.join(".codex/snapshots"), Some(&home), &protected)
                .unwrap()
                .contains("is in")
        );
        assert!(repo_refusal(&data.join("worktrees/api/w1"), Some(&home), &protected).is_some());
        // A symbolic link to the home folder is the home folder.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert!(repo_refusal(&link, Some(&home), &protected).is_some());
    }

    #[test]
    fn marker_scans_stay_linear() {
        // A megabyte of near-markers without a newline.
        let data = b"-----BEGIN X".repeat((1 << 20) / 12);
        let start = std::time::Instant::now();
        assert_eq!(secret_marker(&data), None);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn pages_end_at_line_ends() {
        let text = "a\n".repeat(10_000);
        let (s, e) = page(&text, 0, PAGE);
        assert_eq!(s, 0);
        assert!(e <= PAGE && text[..e].ends_with('\n'));
        let (s, e) = page("héllo", 2, 10);
        assert_eq!((s, e), (3, 6));
    }
}
