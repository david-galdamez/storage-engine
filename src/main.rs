mod cmd;

fn main() -> anyhow::Result<()> {
    Ok(cmd::run()?)
}
