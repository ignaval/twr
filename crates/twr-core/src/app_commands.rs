//! Host-independent CLI argument policy for application clients.
use anyhow::Result;
pub fn validate(args: &[String]) -> Result<()> {
    const ROOTS: &[&str] = &[
        "hub",
        "info",
        "usage",
        "thread",
        "orchestrator",
        "say",
        "send",
        "wait",
        "attach",
        "respond",
        "attention",
        "mode",
        "model",
        "rename",
        "stop",
        "park",
        "trim",
        "resume",
        "compact",
        "delete",
        "trash",
        "log",
        "job",
        "speech",
        "speak",
        "remote",
        "events",
        "context",
        "memory",
        "control",
        "terminal-setup",
        "ws",
        "box",
        "boxes",
        "account",
        "runner",
        "--help",
        "--version",
    ];
    anyhow::ensure!(
        !args.is_empty()
            && args.len() <= 256
            && ROOTS.contains(&args[0].as_str())
            && args.iter().all(|s| s.len() <= 65536 && !s.contains('\0')),
        "Choose a supported twr command"
    );
    anyhow::ensure!(
        args[0] != "hub" || args.get(1).is_some_and(|s| s == "stop"),
        "The app starts an independent hub; a foreground hub cannot run in a terminal"
    );
    let boundary = args
        .iter()
        .position(|s| s == "--")
        .into_iter()
        .chain(
            (args.first().is_some_and(|s| s == "box") && args.get(1).is_some_and(|s| s == "add"))
                .then(|| {
                    args.iter()
                        .position(|s| s == "--command" || s.starts_with("--command="))
                })
                .flatten(),
        )
        .min()
        .unwrap_or(args.len());
    anyhow::ensure!(
        !args[..boundary].iter().any(|s| s == "--socket"
            || s.starts_with("--socket=")
            || s == "--workspace"
            || s.starts_with("--workspace=")
            || s.starts_with("-w")),
        "Commands stay in the selected workspace"
    );
    Ok(())
}
pub fn plan(args: &[String], workspace: &str) -> Result<Vec<String>> {
    validate(args)?;
    if let Some(error) = crate::paths::workspace_name_error(workspace) {
        anyhow::bail!(error)
    }
    let global = matches!(args[0].as_str(), "remote" | "runner")
        || (matches!(args[0].as_str(), "speak" | "speech")
            && args.iter().any(|s| s == "--all-workspaces"));
    let mut out = if global {
        Vec::new()
    } else {
        vec!["-w".into(), workspace.into()]
    };
    out.extend_from_slice(args);
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scope_flags_cannot_change_the_target_but_subprocess_flags_are_preserved() {
        assert!(validate(&["attach".into(), "api".into(), "-wother".into()]).is_err());
        let args = vec![
            "job".into(),
            "run".into(),
            "--".into(),
            "npm".into(),
            "test".into(),
            "--workspace=api".into(),
        ];
        assert_eq!(plan(&args, "default").unwrap()[2..], args);
        assert_eq!(
            plan(&["remote".into(), "server".into()], "default").unwrap(),
            ["remote", "server"]
        );
        let args = vec![
            "box".into(),
            "add".into(),
            "container".into(),
            "--command".into(),
            "podman".into(),
            "--workspace=inside".into(),
        ];
        assert!(validate(&args).is_ok());
    }
}
