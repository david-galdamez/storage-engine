use clap::{Parser, Subcommand};

/// Program for a key - value database
#[derive(Parser, Debug)]
#[command(
    no_binary_name = true,
    name = "storage-engine",
    about,
    long_about = None,
    disable_version_flag = true
)]
pub struct Cmd {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Get { key: String },
    Set { key: String, value: String },
    Delete { key: String },
    Scan,
    Exit,
}
