//! Runs the REPL binary against a file and checks that data persists.

use std::io::Write;
use std::process::{Command, Stdio};

fn run_repl(db: &std::path::Path, input: &str) -> anyhow::Result<String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_storage-engine-rs"))
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?
        .write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "repl exited with {}",
        output.status
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn data_survives_restart_and_eof_ends_repl() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("repl.db");
    run_repl(&db, "set a 1\nexit\n")?;
    // No `exit`: the process must end on EOF.
    let out = run_repl(&db, "get a\n")?;
    anyhow::ensure!(out.contains("$ 1\n"), "unexpected output: {out:?}");
    Ok(())
}
