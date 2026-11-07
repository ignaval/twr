//! Client attachments always resolve their destination from the hub's thread record.
use super::Hub;
use std::path::PathBuf;
use twr_core::proto::{codes, RpcError};
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
        let view = self.get_thread(thread).map_err(|e| reject(e.message))?;
        let id = view["id"]
            .as_str()
            .ok_or_else(|| reject("Thread ID missing".into()))?;
        let root = view["worktree"]
            .as_str()
            .or(view["repo"].as_str())
            .ok_or_else(|| reject("Thread has no working directory".into()))?;
        let name = view["box_name"].as_str().unwrap_or("local");
        let host = self
            .inner
            .host(name)
            .ok_or_else(|| reject("Thread box is unavailable".into()))?;
        let registered_repository = view["worktree"].as_str().is_none();
        host.upload(
            scope,
            id.into(),
            PathBuf::from(root),
            registered_repository,
            request,
        )
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
