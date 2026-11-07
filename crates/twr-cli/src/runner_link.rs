//! User-only runner controls. Pairing codes never appear in command arguments.
use anyhow::{ensure, Result};
use twr_core::paths::Paths;
use twr_link::{bootstrap, service::Request};

pub async fn control(paths: &Paths, request: Request) -> Result<()> {
    super::refuse_agent_action("manage runner links")?;
    if !matches!(request, Request::Stop | Request::Status) {
        bootstrap::start(paths, &std::env::current_exe()?).await?;
    }
    let result = bootstrap::request(paths, &request).await?;
    if let Some(code) = result.get("code").and_then(|v| v.as_str()) {
        // Only this explicit interactive action renders the secret, outside the conversation.
        println!("Pairing code: {code}\n\nOn the other hub, choose Connect a runner or run:\n  twr box pair <name>\n\nThe code expires in 15 minutes and works once.");
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}
pub async fn pair(paths: &Paths, name: String, code_stdin: bool, replace: bool) -> Result<()> {
    super::refuse_agent_action("pair a runner")?;
    twr_core::runner::validate_box_name(&name)?;
    ensure!(
        !twr_core::config::Config::load_with_boxes(&paths.config_file)?
            .boxes
            .contains_key(&name),
        "a box already uses this name"
    );
    let paired_name = name.clone();
    let code = tokio::task::spawn_blocking(move || -> Result<String> {
        use std::io::{BufRead, Read};
        let mut code = String::new();
        if code_stdin {
            std::io::stdin().lock().take(65).read_line(&mut code)?;
        } else {
            code = super::term::read_hidden_line("Runner pairing code: ")?;
        }
        ensure!(code.len() <= 64, "invalid pairing code");
        Ok(code.trim().to_string())
    })
    .await??;
    bootstrap::start(paths, &std::env::current_exe()?).await?;
    let result = bootstrap::request(
        paths,
        &Request::Connect {
            code,
            replace,
            name,
            workspace: paths.workspace_name().into(),
        },
    )
    .await?;
    register(paths, &paired_name).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub async fn manage(paths: &Paths, name: String, admin: twr_link::service::Admin) -> Result<()> {
    super::refuse_agent_action("manage runner access")?;
    twr_link::bootstrap::start(paths, &std::env::current_exe()?).await?;
    let result = twr_link::bootstrap::request(
        paths,
        &Request::Manage {
            name,
            workspace: paths.workspace_name().into(),
            admin,
        },
    )
    .await?;
    if let Some(code) = result["code"].as_str() {
        println!("Pairing code: {code}\n\nOn the new hub, choose Connect a runner or run twr box pair <name>.\nThis code works once and expires in 15 minutes.");
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

pub async fn recover(paths: &Paths, name: String) -> Result<()> {
    super::refuse_agent_action("recover a runner pairing")?;
    bootstrap::start(paths, &std::env::current_exe()?).await?;
    let result = bootstrap::request(
        paths,
        &Request::Recover {
            name: name.clone(),
            workspace: paths.workspace_name().into(),
        },
    )
    .await?;
    register(paths, &name).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
async fn register(paths: &Paths, name: &str) -> Result<()> {
    {
        use twr_core::config::{BoxConfig, BoxTransport, BoxesFile};
        let file = paths.boxes_file();
        let _lock = BoxesFile::lock(&file)?;
        let mut boxes = BoxesFile::load(&file)?;
        ensure!(
            boxes
                .boxes
                .get(name)
                .is_none_or(|b| b.transport() == BoxTransport::Link),
            "runner paired, but a different box has this name; recover after fixing boxes.toml"
        );
        boxes
            .boxes
            .entry(name.to_string())
            .or_insert_with(|| BoxConfig {
                transport: Some(BoxTransport::Link),
                ..Default::default()
            });
        boxes.save(&file)?;
    }
    super::wait_for_hub(&paths.socket, paths).await?;
    let mut hub = super::connect(&paths.socket).await?;
    hub.call("box.add", serde_json::json!({"name":name}))
        .await?;
    Ok(())
}
