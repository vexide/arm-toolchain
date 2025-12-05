use std::{io, sync::LazyLock};

use crate::toolchain::{ToolchainClient, ToolchainError, ToolchainVersion};
use clap::builder::styling;
use humansize::DECIMAL;
use indicatif::ProgressStyle;
use miette::Diagnostic;
use thiserror::Error;
use tokio_util::{future::FutureExt, sync::CancellationToken};

#[derive(Debug, Error, Diagnostic)]
pub enum CliError {
    #[error(transparent)]
    #[diagnostic(code(arm_toolchain::cli::nteractive_prompt_failed))]
    Inquire(#[from] inquire::InquireError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Toolchain(ToolchainError),

    #[error("No ARM toolchain is enabled on this system")]
    #[diagnostic(code(arm_toolchain::cli::no_toolchain_enabled))]
    #[diagnostic(help("Install and activate a toolchain by running the `use latest` subcommand."))]
    NoToolchainEnabled,

    #[error("The toolchain {:?} is not installed.", version.name)]
    #[diagnostic(code(arm_toolchain::cli::toolchain_missing))]
    #[diagnostic(help("Install and activate it by running the `install {version}` subcommand."))]
    ToolchainNotInstalled { version: ToolchainVersion },

    #[error("No ARM toolchains are installed on this system")]
    #[diagnostic(code(arm_toolchain::cli::no_toolchains_installed))]
    #[diagnostic(help("There is nothing to remove."))]
    NoToolchainsToRemove,

    #[error("The toolchain {:?} is not installed.", version.name)]
    #[diagnostic(code(arm_toolchain::cli::remove_missing))]
    CannotRemoveMissingToolchain { version: ToolchainVersion },
}

impl From<ToolchainError> for CliError {
    fn from(value: ToolchainError) -> Self {
        match value {
            // CLI version has a different help message.
            ToolchainError::ToolchainNotInstalled { version } => {
                Self::ToolchainNotInstalled { version }
            }
            other => Self::Toolchain(other),
        }
    }
}

impl From<io::Error> for CliError {
    fn from(value: io::Error) -> Self {
        ToolchainError::from(value).into()
    }
}

/// Arm Toolchain Manager is a tool for installing and managing the LLVM-based ARM embedded toolchain.
///
/// See also: `atrun`
#[derive(Debug, clap::Subcommand)]
pub enum ArmToolchainCmd {
    /// Install, verify, and extract a version of the ARM Embedded Toolchain.
    Install(InstallArgs),
    /// Uninstall a single toolchain version or all versions.
    #[clap(visible_alias("uninstall"))]
    Remove(RemoveArgs),
    /// Run a command with the active toolchain added to the PATH.
    Run(RunArgs),
    /// Print the path of the active toolchain.
    Locate(LocateArgs),
    /// Active a desired version of the ARM Embedded Toolchain, downloading it if necessary.
    Use(UseArgs),
    /// List all installed toolchain versions and the current active version.
    List,
    /// Delete the cache which stores incomplete downloads.
    PurgeCache,
}

impl ArmToolchainCmd {
    pub async fn run(self) -> Result<(), CliError> {
        match self {
            ArmToolchainCmd::Install(config) => {
                install(config).await?;
            }
            ArmToolchainCmd::Remove(args) => {
                remove(args).await?;
            }
            ArmToolchainCmd::Run(args) => {
                run(args).await?;
            }
            ArmToolchainCmd::Locate(args) => {
                locate(args).await?;
            }
            ArmToolchainCmd::Use(args) => {
                use_cmd(args).await?;
            }
            ArmToolchainCmd::List => {
                list().await?;
            }
            ArmToolchainCmd::PurgeCache => {
                purge_cache().await?;
            }
        }

        Ok(())
    }
}

mod install;
pub use install::*;

mod run;
pub use run::*;

mod use_cmd;
pub use use_cmd::*;

mod remove;
pub use remove::*;

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
    let version = args
        .toolchain
        .or_else(|| client.active_toolchain())
        .ok_or(CliError::NoToolchainEnabled)?;

    let toolchain = client.toolchain(&version).await?;

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

pub async fn list() -> Result<(), CliError> {
    let client = ToolchainClient::using_data_dir().await?;

    let active = client.active_toolchain();
    let installed = client.installed_versions().await?;

    println!(
        "Active: {}",
        active
            .map(|v| v.to_string())
            .unwrap_or_else(|| "None".to_string())
    );

    println!();
    println!("Installed:");

    if installed.is_empty() {
        println!("- (None)");
    }

    for version in installed {
        println!("- {version}");
    }

    Ok(())
}

pub async fn purge_cache() -> Result<(), CliError> {
    let client = ToolchainClient::using_data_dir().await?;
    let bytes = client.purge_cache().await?;

    println!(
        "ARM Toolchain download cache purged ({} deleted)",
        humansize::format_size(bytes, DECIMAL)
    );

    Ok(())
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

pub fn ctrl_c_cancel() -> CancellationToken {
    let cancel_token = CancellationToken::new();

    tokio::spawn({
        let cancel_token = cancel_token.clone();
        async move {
            if let Some(wait_result) = tokio::signal::ctrl_c()
                .with_cancellation_token(&cancel_token)
                .await
            {
                // If this resolved to Some, it means that ctrl-c was pressed
                // before the cancel token was invoked through other means.
                // So: tell the user and cancel the token.

                wait_result.unwrap();
                cancel_token.cancel();
                eprintln!("Cancelled.");
            }

            tokio::signal::ctrl_c().await.unwrap();
            std::process::exit(1);
        }
    });

    cancel_token
}

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

pub static PROGRESS_STYLE_EXTRACT_SPINNER: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{spinner:.green} {msg}")
        .expect("progress style valid")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓")
});

pub static PROGRESS_STYLE_EXTRACT: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{percent:>3.bold}% [{bar:40.dim}] {msg} ({eta} remaining)")
        .expect("progress style valid")
        .progress_chars(PROGRESS_CHARS)
});

pub static PROGRESS_STYLE_DELETE_SPINNER: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{spinner:.red} {msg}")
        .expect("progress style valid")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓")
});

pub static PROGRESS_STYLE_DELETE: LazyLock<ProgressStyle> = LazyLock::new(|| {
    ProgressStyle::with_template("{percent:>3.bold}% [{bar:40.red}] {msg} ({eta} remaining)")
        .expect("progress style valid")
        .progress_chars(PROGRESS_CHARS)
});

pub const STYLES: styling::Styles = styling::Styles::styled()
    .header(styling::AnsiColor::Green.on_default().bold())
    .usage(styling::AnsiColor::Green.on_default().bold())
    .literal(styling::AnsiColor::Blue.on_default().bold())
    .placeholder(styling::AnsiColor::Cyan.on_default());
