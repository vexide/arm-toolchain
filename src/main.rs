use arm_toolchain::{cli::{ArmToolchainCmd, InstallArgs, RunArgs, install, run}, toolchain::ToolchainClient};
use clap::Parser;
use tracing_subscriber::{EnvFilter, util::SubscriberInitExt};

#[derive(clap::Parser)]
enum CliArgs {
    #[clap(flatten)]
    Cmd(ArmToolchainCmd),
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt::fmt()
        .pretty()
        .with_env_filter(EnvFilter::from_default_env())
        .finish()
        .init();

    let CliArgs::Cmd(args) = CliArgs::parse();
    args.run().await?;

    Ok(())
}
