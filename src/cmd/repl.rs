use std::io::{BufRead, Write};
use std::ops::Bound;
use std::path::Path;

use clap::Parser;

use storage_engine_rs::storage::{
    btree::{BTree, BTreeEngine, error::BTreeError, verify_file},
    engine::{Direction, EngineCursor, StorageEngine},
    pager::{
        disk::{Pager, PagerOptions},
        error::PagerError,
        types::PageId,
    },
};

use crate::cmd::commands;

/// Errors of the test REPL: bad input, I/O, and pager or engine failures.
#[derive(thiserror::Error, Debug)]
pub enum ReplError {
    #[error("InputError: {0}")]
    Input(String),
    #[error("Parsing Error: {0}")]
    Parse(#[from] clap::Error),
    #[error("Io Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Pager error: {0}")]
    Pager(#[from] PagerError),
    #[error("Engine error: {0}")]
    Engine(#[from] BTreeError),
    #[error("Not a REPL database: the tree root is not page 1")]
    NotAReplDatabase,
}

type ReplResult<T> = Result<T, ReplError>;

/// The REPL tree always lives at page 1: the first page allocated after the
/// file header.
const REPL_ROOT: PageId = PageId::new(1);

/// Opens the database at `path`, creating it if it does not exist.
fn open_or_create(path: &Path) -> ReplResult<(BTreeEngine, BTree)> {
    match Pager::open(path) {
        Ok(pager) => {
            let engine = BTreeEngine::new(pager);
            // shortcut: REPL tree is fixed at page 1, replace with a catalog lookup in Phase 6
            let tree = engine.open_tree(REPL_ROOT)?;
            Ok((engine, tree))
        }
        Err(PagerError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut pager = Pager::create(path, PagerOptions::default())?;
            let tree = BTree::create(&mut pager)?;
            if tree.root() != REPL_ROOT {
                return Err(ReplError::NotAReplDatabase);
            }
            pager.sync()?;
            Ok((BTreeEngine::new(pager), tree))
        }
        Err(e) => Err(e.into()),
    }
}

/// Opens or creates the database at `path` and runs the REPL until `exit`
/// or the end of input.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or created, or if stdin or
/// stdout fails. Errors of a single command are printed and the REPL goes on.
pub fn run(path: &Path) -> ReplResult<()> {
    let (mut engine, tree) = open_or_create(path)?;
    while let Some(line) = read_line()? {
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        let mut out = std::io::stdout().lock();
        match response(line, &mut engine, tree, &mut out) {
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

fn response(
    line: &str,
    engine: &mut BTreeEngine,
    tree: BTree,
    out: &mut impl Write,
) -> ReplResult<bool> {
    let args = shlex::split(line).ok_or(ReplError::Input(String::from("Invalid quoting")))?;
    let cli = commands::Cmd::try_parse_from(args)?;
    match cli.command {
        commands::Command::Set { key, value } => {
            engine.put(tree, key.as_bytes(), value.as_bytes())?;
            engine.sync()?;
            writeln!(out, "OK")?;
        }
        commands::Command::Delete { key } => {
            let message = if engine.delete(tree, key.as_bytes())? {
                engine.sync()?;
                "(deleted)"
            } else {
                "(not found)"
            };
            writeln!(out, "{message}")?;
        }
        commands::Command::Get { key } => {
            let mut buf = Vec::new();
            if engine.get(tree, key.as_bytes(), &mut buf)? {
                writeln!(out, "{}", String::from_utf8_lossy(&buf))?;
            } else {
                writeln!(out, "(nil)")?;
            }
        }
        commands::Command::Scan => {
            let mut cursor =
                engine.scan(tree, Bound::Unbounded, Bound::Unbounded, Direction::Forward)?;
            let mut value = Vec::new();
            while let Some(key) = cursor.next_entry(Some(&mut value))? {
                writeln!(
                    out,
                    "{} : {}",
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(&value)
                )?;
            }
        }
        commands::Command::Verify => {
            let stats = verify_file(engine.pager(), &[tree])?;
            writeln!(
                out,
                "OK height={} entries={} node_pages={} overflow_pages={}",
                stats.height, stats.entries, stats.node_pages, stats.overflow_pages
            )?;
        }
        commands::Command::Exit => {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Reads one line from stdin. Returns `None` at end of input.
fn read_line() -> ReplResult<Option<String>> {
    write!(std::io::stdout(), "$ ")?;
    std::io::stdout().flush()?;
    let mut buffer = String::new();
    if std::io::stdin().lock().read_line(&mut buffer)? == 0 {
        return Ok(None);
    }
    Ok(Some(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> ReplResult<(tempfile::TempDir, BTreeEngine, BTree)> {
        let dir = tempfile::tempdir()?;
        let (engine, tree) = open_or_create(&dir.path().join("t.db"))?;
        Ok((dir, engine, tree))
    }

    /// Clippy denies `assert!` in functions that return `Result`, so tests
    /// compare through this helper and propagate the failure with `?`.
    fn check_eq<L, R>(left: &L, right: &R) -> ReplResult<()>
    where
        L: PartialEq<R> + std::fmt::Debug + ?Sized,
        R: std::fmt::Debug + ?Sized,
    {
        if left == right {
            Ok(())
        } else {
            Err(ReplError::Input(format!("{left:?} != {right:?}")))
        }
    }

    fn run_line(line: &str, engine: &mut BTreeEngine, tree: BTree) -> ReplResult<(bool, String)> {
        let mut out = Vec::new();
        let quit = response(line, engine, tree, &mut out)?;
        let text = String::from_utf8(out).map_err(|e| ReplError::Input(e.to_string()))?;
        Ok((quit, text))
    }

    #[test]
    fn get_missing_key_is_nil() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        check_eq(
            &run_line("get hola", &mut engine, tree)?,
            &(false, String::from("(nil)\n")),
        )?;
        Ok(())
    }

    #[test]
    fn set_then_get() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        check_eq(&run_line("set hola mundo", &mut engine, tree)?.1, &"OK\n")?;
        check_eq(&run_line("get hola", &mut engine, tree)?.1, &"mundo\n")?;
        Ok(())
    }

    #[test]
    fn quoted_key_with_space() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        run_line("set \"con espacio\" valor", &mut engine, tree)?;
        check_eq(
            &run_line("get \"con espacio\"", &mut engine, tree)?.1,
            &"valor\n",
        )?;
        Ok(())
    }

    #[test]
    fn delete_twice() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        run_line("set hola mundo", &mut engine, tree)?;
        check_eq(
            &run_line("delete hola", &mut engine, tree)?.1,
            &"(deleted)\n",
        )?;
        check_eq(
            &run_line("delete hola", &mut engine, tree)?.1,
            &"(not found)\n",
        )?;
        Ok(())
    }

    #[test]
    fn exit_quits() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        check_eq(
            &run_line("exit", &mut engine, tree)?,
            &(true, String::new()),
        )?;
        Ok(())
    }

    #[test]
    fn invalid_command_is_parse_error() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        let result = run_line("foo", &mut engine, tree);
        check_eq(&matches!(result, Err(ReplError::Parse(_))), &true)?;
        Ok(())
    }

    #[test]
    fn unclosed_quote_is_input_error() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        let result = run_line("get \"abc", &mut engine, tree);
        check_eq(&matches!(result, Err(ReplError::Input(_))), &true)?;
        Ok(())
    }

    #[test]
    fn scan_is_sorted() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        for line in ["set b 2", "set a 1", "set c 3"] {
            run_line(line, &mut engine, tree)?;
        }
        check_eq(
            &run_line("scan", &mut engine, tree)?.1,
            &"a : 1\nb : 2\nc : 3\n",
        )?;
        Ok(())
    }

    #[test]
    fn verify_reports_entries() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        check_eq(
            &run_line("verify", &mut engine, tree)?.1,
            &"OK height=1 entries=0 node_pages=1 overflow_pages=0\n",
        )?;
        for line in ["set a 1", "set b 2", "set c 3"] {
            run_line(line, &mut engine, tree)?;
        }
        check_eq(
            &run_line("verify", &mut engine, tree)?.1,
            &"OK height=1 entries=3 node_pages=1 overflow_pages=0\n",
        )?;
        Ok(())
    }

    #[test]
    fn verify_counts_overflow_pages() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        run_line(&format!("set big {}", "x".repeat(5000)), &mut engine, tree)?;
        let text = run_line("verify", &mut engine, tree)?.1;
        check_eq(&text.contains("entries=1 "), &true)?;
        check_eq(&text.contains("overflow_pages=0"), &false)?;
        Ok(())
    }

    #[test]
    fn data_persists_after_reopen() -> ReplResult<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("t.db");
        let (mut engine, tree) = open_or_create(&path)?;
        run_line("set hola mundo", &mut engine, tree)?;
        drop(engine);
        let (mut engine, tree) = open_or_create(&path)?;
        check_eq(&run_line("get hola", &mut engine, tree)?.1, &"mundo\n")?;
        Ok(())
    }

    #[test]
    fn key_too_large_is_engine_error() -> ReplResult<()> {
        let (_dir, mut engine, tree) = open_temp()?;
        let line = format!("set {} v", "k".repeat(100_000));
        let result = run_line(&line, &mut engine, tree);
        check_eq(
            &matches!(
                result,
                Err(ReplError::Engine(BTreeError::KeyTooLarge { .. }))
            ),
            &true,
        )?;
        Ok(())
    }
}
