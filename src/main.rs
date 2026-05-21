mod cli;
mod config;
mod control;
mod error;
mod files;
mod loop_spec;
mod notify;
mod open;
mod rpc;
mod runner;
mod smoke;
mod state;
mod template;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    runner::main_entry().await
}
