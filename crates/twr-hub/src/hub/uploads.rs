//! Client attachments always resolve their destination from the hub's thread record.
use super::Hub;
use std::path::PathBuf;
use twr_core::config::Config;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::ThreadView;

/// Where `view`'s attachments go, and whether that directory may be reached through aliases
/// (symbolic links resolved on the destination host, `upload::resolve_root`): its own
/// worktree (strict), else the registered repository it runs in, else, for the orchestrator,
/// the directory its runtime starts in (`orchestrator.cwd`, `$HOME` by default): the same
/// directory `spawn_config` gives each. A configured directory, like a registered repository,
/// may be named through a link. `None`: a worker that records neither.
pub(super) fn destination(view: &ThreadView, config: &Config) -> Option<(PathBuf, bool)> {
    if let Some(w) = &view.worktree {
        return Some((PathBuf::from(w), false));
    }
    if view.is_orchestrator() {
        return Some((super::orch::orchestrator_cwd(config), true));
    }
    view.repo.as_ref().map(|r| (PathBuf::from(r), true))
}
impl Hub {
    pub(crate) async fn upload(
        &self,
        scope: String,
        thread: &str,
        request: twr_core::upload::Request,
    ) -> Result<serde_json::Value, RpcError> {
        let begin = matches!(request.action, twr_core::upload::Action::Begin { .. });
        let reject = |message: String| {
            RpcError::new(
                if begin {
                    codes::UPLOAD_REJECTED
                } else {
                    codes::BAD_REQUEST
                },
                message,
            )
        };
        let (id, name, destination) = {
            let core = self.inner.core();
            let id = core.resolve(thread).map_err(|e| reject(e.message))?;
            let view = core.view(&id).map_err(|e| reject(e.message))?;
            (
                id.clone(),
                view.box_name.clone(),
                destination(view, &self.inner.config),
            )
        };
        let (root, registered_repository) =
            destination.ok_or_else(|| reject("Thread has no working directory".into()))?;
        let host = self
            .inner
            .host(&name)
            .ok_or_else(|| reject("Thread box is unavailable".into()))?;
        host.upload(scope, id, root, registered_repository, request)
            .await
            .map_err(|e| {
                RpcError::new(
                    if e.downcast_ref::<twr_core::upload::Rejected>().is_some() {
                        codes::UPLOAD_REJECTED
                    } else if e.downcast_ref::<twr_core::upload::Failed>().is_some() {
                        codes::UPLOAD_FAILED
                    } else {
                        codes::BAD_REQUEST
                    },
                    format!("{e:#}"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(role: Option<&str>, repo: Option<&str>, worktree: Option<&str>) -> ThreadView {
        ThreadView {
            role: role.map(Into::into),
            repo: repo.map(Into::into),
            worktree: worktree.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn workers_upload_into_their_worktree_then_their_repository() {
        let c = Config::default();
        assert_eq!(
            destination(&view(None, Some("/r"), Some("/w")), &c),
            Some((PathBuf::from("/w"), false))
        );
        assert_eq!(
            destination(&view(None, Some("/r"), None), &c),
            Some((PathBuf::from("/r"), true))
        );
        assert_eq!(destination(&view(None, None, None), &c), None);
    }

    #[test]
    fn the_orchestrator_uploads_into_its_working_directory() {
        let mut c = Config::default();
        c.orchestrator.cwd = Some("/srv/orch".into());
        assert_eq!(
            destination(&view(Some("orchestrator"), None, None), &c),
            Some((PathBuf::from("/srv/orch"), true))
        );
        if let Some(home) = std::env::var_os("HOME") {
            c.orchestrator.cwd = None;
            assert_eq!(
                destination(&view(Some("orchestrator"), None, None), &c),
                Some((PathBuf::from(home), true))
            );
        }
    }
}
