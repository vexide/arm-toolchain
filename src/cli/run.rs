use std::{env, ffi::OsString, process::exit};

use futures::never::Never;
use tokio::process::Command;

use crate::{
    cli::CliError,
    toolchain::{ToolchainClient, ToolchainVersion},
};

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Toolchain version override (default: the active version)
    #[arg(short = 'T', long)]
    toolchain: Option<ToolchainVersion>,
    /// Disable environment variables set for cross-compilation
    #[arg(long)]
    no_cross_env: bool,
    /// The command to run with the modified environment
    command: OsString,
    /// Arguments to pass to the command.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    args: Vec<OsString>,
}

pub async fn run(args: RunArgs) -> Result<Never, CliError> {
    let client = ToolchainClient::using_data_dir().await?;
    let version = args
        .toolchain
        .or_else(|| client.active_toolchain())
        .ok_or(CliError::NoToolchainEnabled)?;

    let toolchain = client.toolchain(&version).await?;

    let mut path = OsString::from(toolchain.host_bin_dir());
    if let Some(old_path) = env::var_os("PATH") {
        path.push(":");
        path.push(old_path);
    }

    let mut cmd = Command::new(args.command);
    cmd.args(args.args);
    cmd.env("PATH", path);

    if !args.no_cross_env {
        cmd.env("TARGET_CC", "clang");
        cmd.env("TARGET_AR", "llvm-ar");
    }

    let code = cmd.status().await?.code();
    exit(code.unwrap_or(1));
}
