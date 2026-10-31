use std::io::Write;

use clap::Parser;

use crate::{cmd::commands, storage};

#[derive(thiserror::Error, Debug)]
pub enum ReplError {
    #[error("InputError: {0}")]
    Input(String),
    #[error("Parsing Error: {0}")]
    Parse(#[from] clap::Error),
    #[error("Io Error: {0}")]
    Io(#[from] std::io::Error),
}

type ReplResult<T> = Result<T, ReplError>;

pub fn run() -> ReplResult<()> {
    // This is temporal, until implementing the storage engine
    let mut storage = storage::Storage::new();
    loop {
        let line = read_line()?;
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        let mut out = std::io::stdout().lock();
        match response(line, &mut storage, &mut out) {
            Ok(true) => break,
            Ok(false) => {}
            Err(ReplError::Parse(err)) => err.print()?,
            Err(err) => {
                writeln!(out, "{err}")?;
                out.flush()?;
            }
        }
    }
    Ok(())
}

fn response(line: &str, storage: &mut storage::Storage, out: &mut impl Write) -> ReplResult<bool> {
    let args = shlex::split(line).ok_or(ReplError::Input(String::from("Invalid quoting")))?;
    let cli = commands::Cmd::try_parse_from(args)?;
    match cli.command {
        commands::Command::Set { key, value } => {
            storage.set(&key, &value);
            writeln!(out, "OK")?;
        }
        commands::Command::Delete { key } => {
            let message = if storage.delete(&key).is_some() {
                "(deleted)"
            } else {
                "(not found)"
            };
            writeln!(out, "{message}")?;
        }
        commands::Command::Get { key } => {
            writeln!(out, "{}", storage.get(&key).unwrap_or("(nil)",))?;
        }
        commands::Command::Scan => {
            for (key, val) in storage.scan() {
                writeln!(out, "{key} : {val}")?;
            }
        }
        commands::Command::Exit => {
            return Ok(true);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run_line(line: &str, storage: &mut storage::Storage) -> ReplResult<(bool, String)> {
        let mut out = Vec::new();
        let quit = response(line, storage, &mut out)?;
        let text = String::from_utf8(out).map_err(|e| ReplError::Input(e.to_string()))?;
        Ok((quit, text))
    }

    #[test]
    fn get_missing_key_is_nil() {
        let mut storage = storage::Storage::new();
        assert_eq!(
            run_line("get hola", &mut storage).ok(),
            Some((false, String::from("(nil)\n")))
        );
    }

    #[test]
    fn set_then_get() {
        let mut storage = storage::Storage::new();
        assert_eq!(
            run_line("set hola mundo", &mut storage).map(|r| r.1).ok(),
            Some(String::from("OK\n"))
        );
        assert_eq!(
            run_line("get hola", &mut storage).map(|r| r.1).ok(),
            Some(String::from("mundo\n"))
        );
    }

    #[test]
    fn quoted_key_with_space() {
        let mut storage = storage::Storage::new();
        assert!(run_line("set \"con espacio\" valor", &mut storage).is_ok());
        assert_eq!(
            run_line("get \"con espacio\"", &mut storage)
                .map(|r| r.1)
                .ok(),
            Some(String::from("valor\n"))
        );
    }

    #[test]
    fn delete_twice() {
        let mut storage = storage::Storage::new();
        assert!(run_line("set hola mundo", &mut storage).is_ok());
        assert_eq!(
            run_line("delete hola", &mut storage).map(|r| r.1).ok(),
            Some(String::from("(deleted)\n"))
        );
        assert_eq!(
            run_line("delete hola", &mut storage).map(|r| r.1).ok(),
            Some(String::from("(not found)\n"))
        );
    }

    #[test]
    fn exit_quits() {
        let mut storage = storage::Storage::new();
        assert_eq!(
            run_line("exit", &mut storage).ok(),
            Some((true, String::new()))
        );
    }

    #[test]
    fn invalid_command_is_parse_error() {
        let mut storage = storage::Storage::new();
        let result = run_line("foo", &mut storage);
        assert!(matches!(result, Err(ReplError::Parse(_))));
    }

    #[test]
    fn unclosed_quote_is_input_error() {
        let mut storage = storage::Storage::new();
        let result = run_line("get \"abc", &mut storage);
        assert!(matches!(result, Err(ReplError::Input(_))));
    }
}
