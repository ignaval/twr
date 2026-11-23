//! The mobile gateway of the desktop bundle ([`twr_app::gateway`]); `twr gateway` runs the same.
use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = twr_app::gateway::Args::parse();
    twr_app::gateway::run(args, twr_app::gateway::Programs::Bundle).await
}
