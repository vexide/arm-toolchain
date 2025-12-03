use arm_toolchain::cli::{InstallConfig, install};
use clap::Parser;
use tracing_subscriber::{EnvFilter, util::SubscriberInitExt};

#[derive(clap::Parser)]
enum ArmToolchain {
    Install(InstallConfig),
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt::fmt()
        .pretty()
        .with_env_filter(EnvFilter::from_default_env())
        .finish()
        .init();

    let args = ArmToolchain::parse();
    match args {
        ArmToolchain::Install(config) => {
            install(config).await?;
        }
    }

    Ok(())
}
