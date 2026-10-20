//! `config.show`: the configuration the hub runs with, as the orchestrator and users may see
//! it.
//!
//! The view is built field by field from an allowlist, never by serializing [`Config`] and
//! taking secrets out: a box's runner token, every `env` value (`[threads] env`, a project's
//! `env`: they hold API keys as often as build settings), a preset's command lines (which can
//! carry credentials in their arguments), a box's `ssh` destination and the TLS files are
//! never shown, and neither is a runtime given as a command line (`acp:...`, `pty:...`: only its
//! kind). `env` tables show their names only, a box whether it has a token, `[hub] tls`
//! whether it is set. Every struct is destructured without `..`, so a field added to the
//! configuration fails to compile here until someone decides whether it may be shown.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use twr_core::config::{
    BoxConfig, BoxProjectConfig, CodexConfig, Config, HubConfig, NotifyConfig, OrchestratorConfig,
    ProjectConfig, RuntimePreset, ThreadsConfig, TrashConfig, TuiConfig, TuiNotifyConfig,
    DEFAULT_PROJECT_ROOTS,
};

/// A runtime as `config.show` shows it: a name as it is, a command line (`acp:<command>`,
/// `pty:<command>`, which can carry credentials in its arguments) as its kind only.
fn runtime_shown(runtime: &str) -> String {
    match runtime.split_once(':') {
        Some((kind, _)) => format!("{kind}:<command>"),
        None => runtime.to_string(),
    }
}

/// What `config.show` returns for `config`, its boxes those of `boxes` (the hub's, which
/// includes those added since it started; `None`: `config`'s own).
pub fn config_view(config: &Config, boxes: Option<&BTreeMap<String, BoxConfig>>) -> Value {
    let Config {
        default_runtime,
        orchestrator,
        hub,
        boxes: configured,
        projects,
        threads,
        trash,
        stall_after_secs,
        background_timeout_secs,
        wake_after_secs,
        runtimes,
        codex,
        notify: hook,
        tui,
        project_roots,
    } = config;
    let CodexConfig { auto: codex_auto } = codex;
    // The command often holds a secret (an ntfy topic, a webhook's URL): whether it is set.
    let NotifyConfig {
        command: hook_command,
        events: hook_events,
        detail: hook_detail,
        timeout_secs: hook_timeout,
    } = hook;
    let OrchestratorConfig {
        runtime: orch_runtime,
        cwd: orch_cwd,
        autostart,
        model: orch_model,
        effort: orch_effort,
        project_reads,
        read_deny,
    } = orchestrator;
    let HubConfig {
        socket,
        listen,
        tls,
        allow_insecure_remote,
    } = hub;
    let ThreadsConfig {
        thinking,
        default_mode,
        model,
        park_disposable,
        idle_trim_disposable,
        worktree,
        env,
        disk_warn_gb,
        job_slots,
        cpu_weight,
        memory_high,
    } = threads;
    let TrashConfig { keep_days } = trash;
    let TuiConfig {
        mouse,
        animate,
        card_order,
        compact_width,
        notify,
        suggest,
        suggest_model,
    } = tui;
    let TuiNotifyConfig {
        terminal,
        when,
        events,
    } = notify;
    let (roots, roots_default) = match project_roots {
        Some(r) => (json!(r), false),
        None => (json!(DEFAULT_PROJECT_ROOTS), true),
    };
    let projects: Map<String, Value> = projects
        .iter()
        .map(|(name, p)| {
            let ProjectConfig {
                repo,
                runtime,
                box_name,
                worktree,
                env,
                base,
                trim_idle_after,
                idle_trim_disposable,
                codex_auto,
            } = p;
            (
                name.clone(),
                json!({
                    "repo": repo,
                    "runtime": runtime.as_deref().map(runtime_shown),
                    "box": box_name,
                    "worktree": worktree,
                    "base": base,
                    "trim_idle_after": trim_idle_after,
                    "idle_trim_disposable": idle_trim_disposable,
                    "codex_auto": codex_auto.map(|a| a.as_str()),
                    "env_names": env.keys().collect::<Vec<_>>(),
                }),
            )
        })
        .collect();
    let boxes: Map<String, Value> = boxes
        .unwrap_or(configured)
        .iter()
        .map(|(name, b)| {
            // Never its `ssh` or its `command` (one may hold a secret).
            let BoxConfig {
                token,
                ssh: _,
                command: _,
                terminal_command: _,
                transport,
                build: _,
                projects,
            } = b;
            let transport = super::boxes::transport_name(transport.unwrap_or_default());
            let projects: Map<String, Value> = projects
                .iter()
                .map(|(name, p)| {
                    let BoxProjectConfig { repo, runtime } = p;
                    let runtime = runtime.as_deref().map(runtime_shown);
                    (name.clone(), json!({ "repo": repo, "runtime": runtime }))
                })
                .collect();
            (
                name.clone(),
                json!({
                    "transport": transport,
                    "has_token": token.is_some(),
                    "projects": projects,
                }),
            )
        })
        .collect();
    let presets: Vec<&String> = runtimes
        .iter()
        .map(|(name, p)| {
            let RuntimePreset { acp: _, attach: _ } = p;
            name
        })
        .collect();
    json!({
        "note": "the configuration this hub loaded when it started (a change to the file \
                 applies after a hub restart); tokens, env values, preset command lines, \
                 the notify command, ssh destinations, box commands and TLS files are never \
                 shown",
        "default_runtime": runtime_shown(default_runtime),
        "project_roots": roots,
        "project_roots_default": roots_default,
        "stall_after_secs": stall_after_secs,
        "background_timeout_secs": background_timeout_secs,
        "wake_after_secs": wake_after_secs,
        "orchestrator": {
            "runtime": orch_runtime.as_deref().map(runtime_shown),
            "model": orch_model,
            "effort": orch_effort,
            "cwd": orch_cwd,
            "autostart": autostart,
            "project_reads": project_reads,
            "read_deny": read_deny,
        },
        "threads": {
            "default_mode": default_mode,
            "model": model,
            "worktree": worktree,
            "thinking": thinking,
            "park_disposable": park_disposable,
            "idle_trim_disposable": idle_trim_disposable,
            "env_names": env.keys().collect::<Vec<_>>(),
            "disk_warn_gb": disk_warn_gb,
            "job_slots": job_slots,
            "cpu_weight": cpu_weight,
            "memory_high": memory_high,
        },
        "trash": { "keep_days": keep_days },
        "codex": { "auto": codex_auto.as_str() },
        "notify": {
            "command_set": !hook_command.is_empty(),
            "events": hook_events,
            "detail": hook_detail,
            "timeout_secs": hook_timeout,
        },
        "projects": projects,
        "runtimes": presets,
        "boxes": boxes,
        "hub": {
            "socket": socket,
            "listen": listen,
            "tls": tls.is_some(),
            "allow_insecure_remote": allow_insecure_remote,
        },
        "tui": {
            "mouse": mouse,
            "animate": animate,
            "card_order": card_order,
            "compact_width": compact_width,
            "notify": { "terminal": terminal, "when": when, "events": events },
            "suggest": suggest,
            "suggest_model": suggest_model,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_show_and_names_do() {
        let config = Config::from_toml_str(
            r#"
default_runtime = "acp:agent --api-key s3cret-default"
project_roots = ["/home/me/code"]

[orchestrator]
model = "haiku"
runtime = "acp:agent --token s3cret-orch"

[hub]
listen = "0.0.0.0:7878"
tls = { cert = "/etc/twr/hub.pem", key = "/etc/twr/s3cret-key.pem" }

[threads]
env = { API_KEY = "s3cret-env", CARGO_INCREMENTAL = "0" }

[projects.api]
repo = "/home/me/code/api"
env = { STRIPE_KEY = "s3cret-project-env" }
runtime = "pty:tool --password=s3cret-pty"

[projects.web]
repo = "/home/me/code/web"
runtime = "codex"
idle_trim_disposable = ["target/"]

[runtimes.gem]
acp = "gemini --acp --api-key s3cret-cmdline"
attach = "gemini --resume {session} --token s3cret-attach"

[notify]
command = ["curl", "-d", "{body}", "https://ntfy.sh/s3cret-topic"]

[boxes.gpu]
token = "s3cret-token-0123456789abcdef"
ssh = "-i /home/me/.ssh/s3cret_id me@gpu"

[boxes.gpu.projects.api]
repo = "/srv/api"
runtime = "acp:remote --key s3cret-box"
"#,
        )
        .unwrap();
        let v = config_view(&config, None);
        let text = v.to_string();
        assert!(!text.contains("s3cret"), "{text}");
        assert_eq!(v["default_runtime"], "acp:<command>");
        assert_eq!(v["orchestrator"]["runtime"], "acp:<command>");
        assert_eq!(v["projects"]["api"]["runtime"], "pty:<command>");
        assert_eq!(v["projects"]["web"]["runtime"], "codex");
        assert_eq!(
            v["boxes"]["gpu"]["projects"]["api"]["runtime"],
            "acp:<command>"
        );
        assert_eq!(v["orchestrator"]["model"], "haiku");
        assert_eq!(
            v["threads"]["env_names"],
            json!(["API_KEY", "CARGO_INCREMENTAL"])
        );
        assert_eq!(v["projects"]["api"]["env_names"], json!(["STRIPE_KEY"]));
        assert_eq!(v["projects"]["api"]["repo"], "/home/me/code/api");
        assert_eq!(v["runtimes"], json!(["gem"]));
        assert_eq!(v["boxes"]["gpu"]["has_token"], true);
        assert_eq!(v["boxes"]["gpu"]["transport"], "token");
        // The hub's own list of boxes, when given: an SSH or a command box shows its
        // transport, never its destination or its command.
        let ssh: BTreeMap<String, BoxConfig> = BTreeMap::from([
            (
                "far".to_string(),
                BoxConfig {
                    transport: Some(twr_core::config::BoxTransport::Ssh),
                    ssh: Some("-i /home/me/.ssh/s3cret_id me@far".into()),
                    build: Some("0123456789abcdef".into()),
                    ..Default::default()
                },
            ),
            (
                "sb".to_string(),
                BoxConfig {
                    transport: Some(twr_core::config::BoxTransport::Command),
                    command: Some(
                        ["docker", "exec", "-i", "-e", "KEY=s3cret", "ctr"]
                            .map(String::from)
                            .to_vec(),
                    ),
                    terminal_command: Some(
                        ["docker", "exec", "-i", "-t", "ctr"]
                            .map(String::from)
                            .to_vec(),
                    ),
                    ..Default::default()
                },
            ),
        ]);
        let w = config_view(&config, Some(&ssh));
        assert!(
            !w.to_string().contains("s3cret") && !w.to_string().contains("docker"),
            "{w}"
        );
        assert_eq!(w["boxes"]["far"]["transport"], "ssh");
        assert_eq!(w["boxes"]["far"]["has_token"], false);
        assert_eq!(w["boxes"]["sb"]["transport"], "command");
        assert_eq!(w["boxes"]["sb"]["has_token"], false);
        assert!(w["boxes"].get("gpu").is_none(), "{w}");
        assert_eq!(v["boxes"]["gpu"]["projects"]["api"]["repo"], "/srv/api");
        assert_eq!(v["hub"]["tls"], true);
        assert_eq!(v["notify"]["command_set"], true);
        assert_eq!(v["project_roots_default"], false);
        let defaults = config_view(&Config::default(), None);
        assert_eq!(defaults["project_roots_default"], true);
        assert_eq!(defaults["project_roots"], json!(DEFAULT_PROJECT_ROOTS));
        // What idle trims delete: the threads' list, and a project's own.
        assert_eq!(
            defaults["threads"]["idle_trim_disposable"],
            json!(twr_core::config::DEFAULT_IDLE_TRIM_DISPOSABLE)
        );
        assert_eq!(
            v["projects"]["web"]["idle_trim_disposable"],
            json!(["target/"])
        );
        assert_eq!(v["projects"]["api"]["idle_trim_disposable"], Value::Null);
    }
}
