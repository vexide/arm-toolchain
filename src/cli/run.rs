use std::{env, ffi::OsString, process::exit};

use futures::never::Never;
use tokio::process::Command;

use crate::{cli::{CliError, get_toolchain}, toolchain::{ToolchainClient, ToolchainVersion}};

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    #[arg(short = 'T', long)]
    toolchain: Option<ToolchainVersion>,
    command: OsString,
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    args: Vec<OsString>,
}

pub async fn run(args: RunArgs) -> Result<Never, CliError> {
    let client = ToolchainClient::using_data_dir().await?;
    let toolchain = get_toolchain(&client, args.toolchain).await?;

    let mut path = OsString::from(toolchain.host_bin_dir());
    if let Some(old_path) = env::var_os("PATH") {
        path.push(":");
        path.push(old_path);
    }

    let mut cmd = Command::new(args.command);
    cmd.args(args.args);
    cmd.env("PATH", path);

    let code = cmd.status().await?.code();
    exit(code.unwrap_or(1));
}
