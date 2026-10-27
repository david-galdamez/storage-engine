use clap::Parser;

// Program for a key - value database
#[derive(Parser, Debug)]
#[command(version = "0.1", name = "storage-engine", about, long_about = None)]
pub struct Cmd {
    // #[arg(short, long, value_name = "KEY", help = "Key to get from database")]
    // get: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Parser)]
pub enum Command {
    Get { key: String },
    Set { key: String, value: String },
    Delete { key: String },
    Scan,
    Exit,
}
