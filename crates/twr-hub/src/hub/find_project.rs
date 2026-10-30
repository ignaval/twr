//! Finding a project by its repository folder name (`thread.new` `project = "api"` when `api`
//! is neither a configured project nor an absolute path): a git repository named so under the
//! `project_roots`, by the rules of [`twr_core::project_lookup`] (which a box's
//! `twr runner --find-repo` shares). A search cut short by its limits never picks a
//! repository (another of the same name may be in what it skipped).
//!
//! The lookup is the hub machine's: a thread on a remote box never searches it (its project
//! is configured for the box, or an absolute path there; `twr box project` finds or clones
//! it there and records it).

pub(super) use twr_core::project_lookup::{find, is_plain_name};
