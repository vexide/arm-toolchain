use arm_toolchain::cli::{RunArgs, STYLES, run};
use clap::Parser;

/// Run a command with the active ARM Embedded Toolchain added to the PATH.
///
/// See also: `arm-toolchain`
#[derive(Debug, clap::Parser)]
#[clap(version, author, styles(STYLES))]
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
