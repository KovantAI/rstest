use anyhow::Result;

fn main() -> Result<()> {
    std::process::exit(rstest_cli::run()?);
}
