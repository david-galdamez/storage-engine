mod cmd;

fn main() -> anyhow::Result<()> {
    let path = std::env::args_os().nth(1).map_or_else(
        || std::path::PathBuf::from("repl.db"),
        std::path::PathBuf::from,
    );
    Ok(cmd::run(&path)?)
}
