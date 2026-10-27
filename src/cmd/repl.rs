use std::io::Write;

use clap::Parser;

use crate::{
    cmd::commands,
    storage::{self, Storage},
};

#[derive(thiserror::Error, Debug)]
enum ReplError {
    #[error("InputErro: {0}")]
    InputError(String),
    #[error("Parsing Error: {0}")]
    ParseError(#[from] clap::Error),
    #[error("Io Error: {0}")]
    IoError(#[from] std::io::Error),
}

type ReplResult<T> = Result<T, ReplError>;

pub fn run() -> ReplResult<()> {
    // This is temporal, until implementing the storage engine
    let storage = storage::Storage::new();
    loop {
        let line = read_line()?;
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        match response(line, &storage) {
            Ok(quit) => {
                if quit {
                    break;
                }
            }
            Err(err) => {
                write!(std::io::stdout(), "{err}")?;
                std::io::stdout().flush()?;
            }
        }
    }
    Ok(())
}

fn response(line: &str, storage: &Storage) -> ReplResult<bool> {
    let args = shlex::split(line).ok_or(ReplError::InputError(String::from("Invalid quoting")))?;
    let cli = commands::Cmd::try_parse_from(args)?;
    match cli.command {
        commands::Command::Set { key, value } => storage.set(&key, &value),
    }

    Ok(false)
}

fn read_line() -> ReplResult<String> {
    write!(std::io::stdout(), "$ ")?;
    std::io::stdout().flush()?;
    let mut buffer = String::new();
    std::io::stdin().read_line(&mut buffer)?;
    Ok(buffer)
}
