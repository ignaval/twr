//! Gateway upload metadata; files are staged by the authenticated thread host.
pub use twr_core::upload::{Quota, MAX_FILE};
pub struct Pending {
    pub workspace: String,
    pub thread: String,
    pub sha256: String,
    pub size: usize,
    pub created: std::time::Instant,
    pub committed: bool,
    pub busy: bool,
}

/// Only a typed hub rejection proves a new begin did not accept a reservation.
pub fn definite_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<twr_core::proto::RpcError>()
        .is_some_and(|e| e.code == twr_core::proto::codes::UPLOAD_REJECTED)
}
pub fn finish_begin(
    pending: &mut std::collections::HashMap<String, Pending>,
    quota: &mut Quota,
    id: &str,
    reserved: bool,
    result: &anyhow::Result<serde_json::Value>,
) {
    if reserved && result.as_ref().is_err_and(definite_rejection) {
        pending.remove(id);
        quota.release(id);
    } else {
        finish_command(pending, quota, id, "upload-begin", result);
    }
}
pub fn finish_command(
    pending: &mut std::collections::HashMap<String, Pending>,
    quota: &mut Quota,
    id: &str,
    method: &str,
    result: &anyhow::Result<serde_json::Value>,
) {
    if method == "upload-commit"
        && result.as_ref().is_err_and(|error| {
            error
                .downcast_ref::<twr_core::proto::RpcError>()
                .is_some_and(|e| e.code == twr_core::proto::codes::UPLOAD_FAILED)
        })
        && pending.get(id).is_some_and(|upload| !upload.committed)
    {
        pending.remove(id);
        quota.release(id);
        return;
    }
    if let Some(upload) = pending.get_mut(id) {
        upload.busy = false;
        if result.as_ref().is_ok_and(|value| value["path"].is_string()) {
            if !upload.committed {
                quota.commit(id, std::time::Instant::now());
            }
            upload.committed = true;
        }
        if method == "upload-cancel"
            && result
                .as_ref()
                .is_ok_and(|value| value["cancelled"] == true)
            && !upload.committed
        {
            pending.remove(id);
            quota.release(id);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn pending() -> Pending {
        Pending {
            workspace: "default".into(),
            thread: "thread".into(),
            sha256: "a".repeat(64),
            size: 5,
            created: std::time::Instant::now(),
            committed: false,
            busy: true,
        }
    }
    #[test]
    fn definite_begin_rejections_refund_new_slots_but_uncertain_results_do_not() {
        let mut uploads = std::collections::HashMap::new();
        let mut quota = Quota::default();
        let now = std::time::Instant::now();
        for index in 0..8 {
            let id = index.to_string();
            uploads.insert(id.clone(), pending());
            quota.reserve(&id, 5, now).unwrap();
            let result = Err(twr_core::proto::RpcError::new(
                twr_core::proto::codes::UPLOAD_REJECTED,
                "invalid name",
            )
            .into());
            finish_begin(&mut uploads, &mut quota, &id, true, &result);
        }
        assert!(uploads.is_empty());
        quota.reserve("remaining", 128 * 1024 * 1024, now).unwrap();
        quota.release("remaining");
        uploads.insert("uncertain".into(), pending());
        quota.reserve("uncertain", 5, now).unwrap();
        finish_begin(
            &mut uploads,
            &mut quota,
            "uncertain",
            true,
            &Err(anyhow::anyhow!("Companion closed; result unknown")),
        );
        assert!(uploads.contains_key("uncertain"));
        assert!(!uploads["uncertain"].busy);
        assert!(quota.reserve("remaining", 128 * 1024 * 1024, now).is_err());
        let rejected = Err(twr_core::proto::RpcError::new(
            twr_core::proto::codes::UPLOAD_REJECTED,
            "retry rejected",
        )
        .into());
        finish_begin(&mut uploads, &mut quota, "uncertain", false, &rejected);
        assert!(uploads.contains_key("uncertain"));
    }
    #[test]
    fn only_definitive_failed_commits_refund_gateway_reservations() {
        let mut uploads = std::collections::HashMap::from([("file".into(), pending())]);
        let mut quota = Quota::default();
        let now = std::time::Instant::now();
        quota.reserve("file", 5, now).unwrap();
        finish_command(
            &mut uploads,
            &mut quota,
            "file",
            "upload-commit",
            &Err(anyhow::anyhow!("Reply lost; result unknown")),
        );
        assert!(uploads.contains_key("file"));
        let rejected = Err(twr_core::proto::RpcError::new(
            twr_core::proto::codes::UPLOAD_FAILED,
            "Checksum mismatch; staging removed",
        )
        .into());
        finish_command(&mut uploads, &mut quota, "file", "upload-commit", &rejected);
        assert!(uploads.is_empty());
        quota.reserve("remaining", 128 * 1024 * 1024, now).unwrap();
    }
    #[test]
    fn cancellation_after_a_lost_commit_ack_retains_receipt_and_quota() {
        let mut uploads = std::collections::HashMap::from([("file".into(), pending())]);
        let mut quota = Quota::default();
        let now = std::time::Instant::now();
        quota.reserve("file", 5, now).unwrap();
        finish_command(
            &mut uploads,
            &mut quota,
            "file",
            "upload-cancel",
            &Ok(json!({"cancelled":false,"committed":true,"path":"/thread/file"})),
        );
        assert!(uploads["file"].committed);
        assert!(!uploads["file"].busy);
        quota
            .reserve(
                "remaining",
                128 * 1024 * 1024 - twr_core::upload::MIN_FILE_CHARGE,
                now,
            )
            .unwrap();
        assert!(quota.reserve("overflow", 1, now).is_err());
    }
}
