use std::{sync::Arc, time::Duration};

use indicatif::{MultiProgress, ProgressBar};
use inquire::Confirm;
use owo_colors::OwoColorize;
use tokio::task::spawn_blocking;
use tokio_util::sync::CancellationToken;

use crate::{
    cli::{
        CliError, PROGRESS_STYLE_DL, PROGRESS_STYLE_EXTRACT, PROGRESS_STYLE_EXTRACT_SPINNER, PROGRESS_STYLE_VERIFY, ctrl_c_cancel, msg
    },
    toolchain::{
        HostArch, HostOS, InstallState, ToolchainClient, ToolchainError, ToolchainRelease,
        ToolchainVersion,
    },
};

#[derive(Debug, clap::Parser)]
pub struct InstallArgs {
    /// Version of the toolchain to install
    pub version: Option<ToolchainVersion>,
    /// Skip install if toolchain is up-to-date.
    #[clap(long, short)]
    pub force: bool,
}

pub async fn install(args: InstallArgs) -> Result<(), CliError> {
    let client = ToolchainClient::using_data_dir().await?;

    // If "latest" specified we have to figure out what that actually means first
    let toolchain_release;
    let toolchain_version;
    let install_latest;

    if let Some(version) = args.version
        && version.name != "latest"
    {
        install_latest = false;
        toolchain_version = version;
        toolchain_release = client.get_release(&toolchain_version).await?;
    } else {
        install_latest = true;
        toolchain_release = client.latest_release().await?;
        toolchain_version = toolchain_release.version().to_owned();
    }

    if !args.force {
        let already_installed = client.install_path_for(&toolchain_version);
        if already_installed.exists() {
            println!(
                "Toolchain already installed: {} at {}",
                toolchain_version.to_string().bold(),
                already_installed.display().green()
            );

            if client.active_toolchain().as_ref() == Some(&toolchain_version) {
                println!(
                    "(Enable it with the `use {}` subcommand)",
                    if install_latest {
                        "latest".to_string()
                    } else {
                        toolchain_version.to_string()
                    }
                );
            }

            return Ok(());
        }
    }

    confirm_install(&toolchain_version, install_latest).await?;

    let old_version = client.active_toolchain();

    let token = ctrl_c_cancel();
    install_with_progress_bar(&client, &toolchain_release, token.clone()).await?;

    if old_version.is_none() {
        msg!("Activated", "{toolchain_version}");
    }

    token.cancel();
    Ok(())
}

pub async fn confirm_install(version: &ToolchainVersion, latest: bool) -> Result<(), CliError> {
    let confirm_message = format!(
        "Download & install {}ARM toolchain {version}?",
        if latest { "latest " } else { "" },
    );

    let confirmation = spawn_blocking(move || {
        Confirm::new(&confirm_message)
            .with_default(true)
            .with_help_message("Required support libraries for building C/C++ code. No = cancel")
            .prompt()
    })
    .await
    .unwrap()?;

    if !confirmation {
        eprintln!("Cancelled.");
        return Err(ToolchainError::Cancelled)?;
    }

    Ok(())
}

pub async fn install_with_progress_bar(
    client: &ToolchainClient,
    release: &ToolchainRelease,
    cancel_token: CancellationToken,
) -> Result<(), CliError> {
    let asset = release.asset_for(HostOS::current(), HostArch::current())?;

    msg!("Downloading", "{}", asset.name,);

    let multi_bar = MultiProgress::new();
    let download_bar = ProgressBar::no_length().with_style(PROGRESS_STYLE_DL.clone());
    multi_bar.add(download_bar.clone());

    let verify_bar = ProgressBar::no_length()
        .with_style(PROGRESS_STYLE_VERIFY.clone())
        .with_message("Verifying");
    multi_bar.add(verify_bar.clone());

    let extract_bar = ProgressBar::no_length()
        .with_message("Extracting toolchain")
        .with_style(PROGRESS_STYLE_EXTRACT_SPINNER.clone());
    multi_bar.add(extract_bar.clone());

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
            extract_bar.set_style(PROGRESS_STYLE_EXTRACT_SPINNER.clone());
            extract_bar.enable_steady_tick(Duration::from_millis(300));
        }
        InstallState::ExtractCopy {
            bytes_copied,
            total_size,
        } => {
            if extract_bar.length().is_none() {
                extract_bar.set_style(PROGRESS_STYLE_EXTRACT.clone());
                extract_bar.reset();
            }

            extract_bar.set_length(total_size);
            extract_bar.set_position(bytes_copied);
        }
        InstallState::ExtractCleanUp => {}
        InstallState::ExtractDone => {
            extract_bar.finish_with_message("Extraction complete");
        }
    });

    let destination = client
        .download_and_install(release, asset, progress_handler, cancel_token)
        .await?;

    msg!("Downloaded", "to {}", destination.display());

    Ok(())
}
