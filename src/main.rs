mod cmd;
mod storage;

fn main() -> anyhow::Result<()> {
    cmd::run()?;
}
