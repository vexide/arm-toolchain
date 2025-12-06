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
    #[diagnostic(code(arm_toolchain::cli::interactive_prompt_failed))]
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
    ///
    /// Toolchains are installed per-user in a platform-specific data directory.
    /// If there is another toolchain already installed, that toolchain will still
    /// be used after installing this one.
    ///
    /// If you would like to enable a toolchain you've installed, or install and enable
    /// a toolchain all at once, invoke the `use` command instead.
    #[clap(
        visible_alias("add"),
        visible_alias("i"),
    )]
    Install(InstallArgs),
    /// Uninstall a single toolchain version, or all versions.
    ///
    /// When a toolchain is uninstalled, it is unset as the current toolchain and deleted
    /// from the toolchains directory and download cache.
    ///
    /// If "all" is specified as the version to remove, every toolchain on the system will be
    /// uninstalled.
    #[clap(
        visible_alias("uninstall"),
        visible_alias("rm"),
    )]
    Remove(RemoveArgs),
    /// Run a command with the active toolchain added to the `PATH`.
    ///
    /// Unless you specify `--no-cross-env`, the `TARGET_CC` and `TARGET_AR` environment
    /// variables will also be set to `clang` and `llvm-ar` respectively. These will resolve
    /// to the toolchain's versions of clang and llvm-ar.
    ///
    /// An alias for this command is the external `atrun` executable. You may need to pass an
    /// extra `--` to the command if some flags look like ones `arm-toolchain` would accept.
    Run(RunArgs),
    /// Print the path of the active toolchain.
    #[clap(
        visible_alias("which"),
        visible_alias("where"),
        visible_alias("print"),
    )]
    Locate(LocateArgs),
    /// Active a desired version of the ARM Embedded Toolchain, downloading it if necessary.
    #[clap(
        visible_alias("set"),
        visible_alias("activate"),
    )]
    Use(UseArgs),
    /// List all installed toolchain versions and the current active version.
    #[clap(visible_alias("ls"))]
    List,
    /// Delete the cache which stores incomplete downloads.
    PurgeCache,
}

impl ArmToolchainCmd {
    /// Run the command.
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

/// Options for locating a toolchain.
#[derive(Debug, clap::Args)]
pub struct LocateArgs {
    /// The toolchain that should be located.
    #[arg(short = 'T', long)]
    toolchain: Option<ToolchainVersion>,
    /// Which path should be displayed.
    #[clap(default_value = "install-dir")]
    what: LocateWhat,
}

#[derive(Debug, Clone, Default, PartialEq, clap::ValueEnum)]
enum LocateWhat {
    /// The root directory, where the toolchain is installed.
    #[default]
    InstallDir,
    /// The `/bin` directory, where executables are stored (e.g. clang).
    Bin,
    /// The `/lib` directory, where libraries are stored (e.g. libLTO.dylib).
    Lib,
    /// The multilib directory, where cross-compilation libraries are stored
    /// for various platforms (e.g. libc.a).
    Multilib,
}

/// Locate a toolchain's path and print it to stdio.
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

/// Print a list of all toolchains to stdio.
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

/// Purge the download cache and print results to stdio.
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

/// Create a cancel token that will trigger when Ctrl-C (SIGINT on Unix) is pressed.
///
/// If the token is cancelled manually, Ctrl-C's behavior will return to exiting the
/// process. It is advised to not call this function in a loop because it creates a
/// Tokio task that only exits after Ctrl-C is pressed twice.
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
