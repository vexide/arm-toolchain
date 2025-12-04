use std::{env, ffi::OsString, process::exit};

use futures::never::Never;
use tokio::process::Command;

use crate::{cli::{CliError, get_toolchain}, toolchain::{ToolchainClient, ToolchainVersion}};

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Toolchain version override (default: the active version)
    #[arg(short = 'T', long)]
    toolchain: Option<ToolchainVersion>,
    /// Set environment variables for cross-compilation
    #[arg(long, default_value_t = true)]
    cross_env: bool,
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
    let toolchain = get_toolchain(&client, args.toolchain).await?;

    let mut path = OsString::from(toolchain.host_bin_dir());
    if let Some(old_path) = env::var_os("PATH") {
        path.push(":");
        path.push(old_path);
    }

    let mut cmd = Command::new(args.command);
    cmd.args(args.args);
    cmd.env("PATH", path);

    if args.cross_env {
        cmd.env("TARGET_CC", "clang");
        cmd.env("CC", "clang");

        cmd.env("TARGET_AR", "llvm-ar");
        cmd.env("AR", "llvm-ar");
    }

    let code = cmd.status().await?.code();
    exit(code.unwrap_or(1));
}
