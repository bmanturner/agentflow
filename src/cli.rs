use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "agentflow", version, about = "Tiny local loop runner for OMP")]
pub struct Cli {
    #[arg(long, global = true, default_value = ".agentflow.yml")]
    pub config: PathBuf,

    #[arg(long, global = true, default_value = ".")]
    pub repo_root: PathBuf,

    #[arg(long, global = true, default_value = "omp")]
    pub omp: String,

    #[arg(long, global = true)]
    pub state: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Run(RunArgs),
    Status,
    Open,
    Resume(ResumeArgs),
    Notify(MessageArgs),
    Halt(MessageArgs),
    SmokeTest,
}

#[derive(Debug, Args, Clone, Copy, Default)]
pub struct RunArgs {
    #[arg(long)]
    pub start: Option<i64>,

    #[arg(long)]
    pub end: Option<i64>,

    #[arg(long)]
    pub count: Option<u64>,
}

#[derive(Debug, Args)]
pub struct ResumeArgs {
    pub message: Option<String>,
}

#[derive(Debug, Args)]
pub struct MessageArgs {
    #[arg(long)]
    pub message: String,
}
