mod ipc;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "osm", version, about = "Omarchy session memory engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report engine health as JSON
    Status {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Status { json: _ } => {
            let report = ipc::StatusReport::new(true, None);
            println!("{}", serde_json::to_string(&report)?);
        }
    }
    Ok(())
}
