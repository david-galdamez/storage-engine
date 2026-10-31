mod cmd;
mod storage;

fn main() -> anyhow::Result<()> {
    Ok(cmd::run()?)
}
