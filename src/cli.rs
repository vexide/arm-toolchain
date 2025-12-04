use std::{io, sync::LazyLock};

use crate::toolchain::{InstalledToolchain, ToolchainClient, ToolchainError, ToolchainVersion};
use indicatif::ProgressStyle;
use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum CliError {
    #[error(transparent)]
    #[diagnostic(code(arm_toolchain::cli::nteractive_prompt_failed))]
    Inquire(#[from] inquire::InquireError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Toolchain(#[from] ToolchainError),

    #[error("No ARM toolchain is enabled on this system")]
    #[diagnostic(code(arm_toolchain::cli::no_toolchain_enabled))]
    #[diagnostic(help("Use the `install` command to download and install a toolchain."))]
    NoToolchainEnabled,

    #[error("The toolchain {name:?} is not installed.")]
    #[diagnostic(code(arm_toolchain::cli::toolchain_missing))]
    #[diagnostic(help("Use the `install` command to download and install a toolchain."))]
    ToolchainNotInstalled { name: String },
}

impl From<io::Error> for CliError {
    fn from(value: io::Error) -> Self {
        ToolchainError::from(value).into()
    }
}

#[derive(Debug, clap::Subcommand)]
pub enum ArmToolchainCmd {
    Install(InstallArgs),
    Run(RunArgs),
    Locate(LocateArgs),
}

impl ArmToolchainCmd {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            ArmToolchainCmd::Install(config) => {
                install(config).await?;
            }
            ArmToolchainCmd::Run(args) => {
                run(args).await?;
            }
            ArmToolchainCmd::Locate(args) => {
                locate(args).await?;
            }
        }

        Ok(())
    }
}

mod install;
pub use install::*;

mod run;
pub use run::*;

#[derive(Debug, clap::Args)]
pub struct LocateArgs {
    #[arg(short = 'T', long)]
    toolchain: Option<ToolchainVersion>,
    #[clap(default_value = "install-dir")]
    what: LocateWhat,
}

#[derive(Debug, Clone, Default, PartialEq, clap::ValueEnum)]
enum LocateWhat {
    #[default]
    InstallDir,
    Bin,
    Lib,
    Multilib,
}

pub async fn locate(args: LocateArgs) -> Result<(), CliError> {
    let client = ToolchainClient::using_data_dir().await?;
    let toolchain = get_toolchain(&client, args.toolchain).await?;

    match args.what {
        LocateWhat::InstallDir => {
            println!("{}", toolchain.path.display());
        }
        LocateWhat::Bin => {
            println!("{}", toolchain.host_bin_dir().display());
        }
        LocateWhat::Lib => {
            println!("{}", toolchain.lib_dir().display());
        }
        LocateWhat::Multilib => {
            println!("{}", toolchain.multilib_dir().display());
        }
    }

    Ok(())
}

pub async fn get_toolchain(
    client: &ToolchainClient,
    version: Option<ToolchainVersion>,
) -> Result<InstalledToolchain, CliError> {
    let version = version
        .or_else(|| client.current_version())
        .ok_or(CliError::NoToolchainEnabled)?;

    let installed_toolchains = client.installed_versions().await?;
    if !installed_toolchains.contains(&version) {
        return Err(CliError::ToolchainNotInstalled { name: version.name });
    }

    Ok(client.toolchain(&version))
}

macro_rules! msg {
    ($label:expr, $($rest:tt)+) => {
        {
            use owo_colors::OwoColorize;
            eprintln!("{:>12} {}", $label.green().bold(), format_args!($($rest)+))
        }
    };
}
pub(crate) use msg;

const PROGRESS_CHARS: &str = "=> ";

pub static PROGRESS_STYLE_DL: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{percent:>3.bold}% [{bar:40.blue}] ({bytes}/{total_bytes}, {eta} remaining) {bytes_per_sec}")
    .expect("progress style valid")
    .progress_chars(PROGRESS_CHARS)
});

pub static PROGRESS_STYLE_DL_MSG: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{percent:>3.bold}% [{bar:40.blue}] ({bytes}/{total_bytes}) {msg}")
        .expect("progress style valid")
        .progress_chars(PROGRESS_CHARS)
});

pub static PROGRESS_STYLE_VERIFY: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{percent:>3.bold}% [{bar:40.green}] {msg} ({eta} remaining)")
        .expect("progress style valid")
        .progress_chars(PROGRESS_CHARS)
});

pub static PROGRESS_STYLE_EXTRACT: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{percent:>3.bold}% [{bar:40.dim}] {msg} ({eta} remaining)")
        .expect("progress style valid")
        .progress_chars(PROGRESS_CHARS)
});

pub static PROGRESS_STYLE_SPINNER: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{spinner:.green} {msg}")
        .expect("progress style valid")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓")
});
