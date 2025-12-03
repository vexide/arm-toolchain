use std::{
    process::exit,
    sync::{Arc, LazyLock},
    time::Duration,
};

use crate::toolchain::{HostArch, HostOS, InstallState, ToolchainClient, ToolchainError, ToolchainVersion};
use indicatif::{ProgressBar, ProgressStyle};
use inquire::Confirm;
use miette::Diagnostic;
use owo_colors::OwoColorize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error, Diagnostic)]
pub enum CliError {
    #[error(transparent)]
    #[diagnostic(code(arm_toolchain::interactive_prompt_failed))]
    Inquire(#[from] inquire::InquireError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Toolchain(#[from] ToolchainError),
}

#[derive(Debug, clap::Parser)]
pub struct InstallConfig {
    /// Version of LLVM to install (`None` = latest).
    pub llvm_version: Option<ToolchainVersion>,
    /// Skip install if toolchain is up-to-date.
    #[clap(long, short)]
    pub force: bool,
}

macro_rules! msg {
    ($label:expr, $($rest:tt)+) => {
        {
            use owo_colors::OwoColorize;
            eprintln!("{:>12} {}", $label.green().bold(), format_args!($($rest)+))
        }
    };
}

pub async fn install(cfg: InstallConfig) -> Result<(), CliError> {
    // let project = Project::find().await?;
    let toolchain = ToolchainClient::using_data_dir().await?;

    let toolchain_release;
    let confirm_message;
    let toolchain_version;

    if let Some(mut version) = cfg.llvm_version
        && version.name != "latest"
    {
        if let Some(bare) = version.name.strip_prefix("v") {
            version.name = bare.to_string();
        }

        toolchain_version = version;
        toolchain_release = toolchain.get_release(&toolchain_version).await?;
        confirm_message = format!("Download & install LLVM toolchain {toolchain_version}?");
    } else {
        toolchain_release = toolchain.latest_release().await?;
        toolchain_version = toolchain_release.version().to_owned();
        confirm_message =
            format!("Download & install latest LLVM toolchain ({toolchain_version})?");
    }

    if !cfg.force {
        let already_installed = toolchain.install_path_for(&toolchain_version);
        if already_installed.exists() {
            println!(
                "Toolchain up-to-date: {} at {}",
                toolchain_version.to_string().bold(),
                already_installed.display().green()
            );
            return Ok(());
        }
    }

    let confirmation = Confirm::new(&confirm_message)
        .with_default(true)
        .with_help_message("Required support libraries for building C/C++ code. No = cancel")
        .prompt()?;

    if !confirmation {
        eprintln!("Cancelled.");
        exit(1);
    }

    let asset = toolchain_release.asset_for(HostOS::current(), HostArch::current())?;

    msg!(
        "Downloading",
        "{} <{}>",
        asset.name.bold(),
        asset.browser_download_url.green()
    );

    let cancel_token = CancellationToken::new();

    tokio::spawn({
        let cancel_token = cancel_token.clone();
        async move {
            tokio::signal::ctrl_c().await.unwrap();
            cancel_token.cancel();
            eprintln!("Cancelled.");

            tokio::signal::ctrl_c().await.unwrap();
            std::process::exit(1);
        }
    });

    let download_bar = ProgressBar::no_length().with_style(PROGRESS_STYLE_DL.clone());
    let verify_bar = ProgressBar::no_length()
        .with_style(PROGRESS_STYLE_VERIFY.clone())
        .with_message("Verifying");
    let extract_bar = ProgressBar::no_length()
        .with_message("Extracting toolchain")
        .with_style(PROGRESS_STYLE_SPINNER.clone());

    let progress_handler = Arc::new(move |update| match update {
        InstallState::DownloadBegin {
            asset_size,
            bytes_read,
        } => {
            download_bar.reset();
            download_bar.enable_steady_tick(Duration::from_millis(300));
            download_bar.set_length(asset_size);
            download_bar.set_position(bytes_read);
            download_bar.reset_eta();
        }
        InstallState::Download { bytes_read } => {
            download_bar.set_position(bytes_read);
        }
        InstallState::DownloadFinish => {
            download_bar.disable_steady_tick();
            download_bar.finish_with_message("Download complete");
        }
        InstallState::VerifyingBegin { asset_size } => {
            verify_bar.reset();
            verify_bar.set_length(asset_size);
        }
        InstallState::Verifying { bytes_read } => {
            verify_bar.set_position(bytes_read);
        }
        InstallState::VerifyingFinish => {
            verify_bar.finish_with_message("Verification complete");
        }
        InstallState::ExtractBegin => {
            extract_bar.set_style(PROGRESS_STYLE_SPINNER.clone());
            extract_bar.enable_steady_tick(Duration::from_millis(300));
        }
        InstallState::ExtractCopy(transit_process) => {
            if extract_bar.length().is_none() {
                extract_bar.set_style(PROGRESS_STYLE_EXTRACT.clone());
                extract_bar.reset();
            }

            extract_bar.set_length(transit_process.total_bytes);
            extract_bar.set_position(transit_process.copied_bytes);
        }
        InstallState::ExtractCleanUp => {}
        InstallState::ExtractDone => {
            extract_bar.finish_with_message("Extraction complete");
        }
    });

    let destination = toolchain
        .download_and_install(&toolchain_release, asset, progress_handler, cancel_token)
        .await?;
    msg!("Downloaded", "to {}", destination.display());

    Ok(())
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
