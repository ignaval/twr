//! Private stdio bridge of the desktop app and the mobile gateway ([`twr_app::bridge`]).
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = std::env::args().nth(1).unwrap_or_else(|| "default".into());
    twr_app::bridge::run(workspace).await
}
