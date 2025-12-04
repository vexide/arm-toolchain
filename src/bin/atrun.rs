use arm_toolchain::cli::{RunArgs, run};
use clap::Parser;

#[derive(Debug, clap::Parser)]
struct Args {
    #[clap(flatten)]
    run_args: RunArgs,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let args = Args::parse();
    run(args.run_args).await?;
    Ok(())
}
