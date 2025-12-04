use futures::future::try_join_all;
use humansize::DECIMAL;
use indicatif::{MultiProgress, ProgressBar};
use tokio_util::sync::CancellationToken;

use crate::{
    cli::{CliError, PROGRESS_STYLE_DELETE, PROGRESS_STYLE_DELETE_SPINNER, ctrl_c_cancel, msg},
    toolchain::{RemoveProgress, ToolchainClient, ToolchainError, ToolchainVersion},
};

#[derive(Debug, clap::Parser)]
pub struct RemoveArgs {
    /// Version of toolchain to remove, or "all"
    pub version: ToolchainVersion,
}

pub async fn remove(args: RemoveArgs) -> Result<(), CliError> {
    let client = ToolchainClient::using_data_dir().await?;
    let toolchains = client.installed_versions().await?;

    if args.version.name == "all" {
        let old_active = client.active_toolchain();
        client.set_active_toolchain(None).await?;

        if toolchains.is_empty() && old_active.is_none() {
            return Err(CliError::NoToolchainEnabled);
        }

        let cancel_token = ctrl_c_cancel();
        let multi_progress = MultiProgress::new();
        let mut futs = vec![];

        for version in toolchains {
            let client = client.clone();
            let tok = cancel_token.clone();
            let multi_progress = multi_progress.clone();

            futs.push(remove_with_progress_bar(
                client,
                version,
                tok,
                multi_progress,
            ));
        }

        let out = try_join_all(futs).await?;
        let total_bytes = out.iter().sum::<u64>();

        println!(
            "Removed {} toolchains ({})",
            out.len(),
            humansize::format_size(total_bytes, DECIMAL),
        );

        cancel_token.cancel();
    } else {
        if !toolchains.contains(&args.version) {
            return Err(CliError::CannotRemoveMissingToolchain {
                version: args.version,
            });
        }

        let cancel_token = ctrl_c_cancel();
        let multi = MultiProgress::new();
        let bytes =
            remove_with_progress_bar(client, args.version.clone(), cancel_token.clone(), multi)
                .await?;

        cancel_token.cancel();

        msg!(
            "Removed",
            "{} ({})",
            args.version,
            humansize::format_size(bytes, DECIMAL),
        );
    }

    Ok(())
}

async fn remove_with_progress_bar(
    client: ToolchainClient,
    version: ToolchainVersion,
    cancel_token: CancellationToken,
    multi_progress: MultiProgress,
) -> Result<u64, ToolchainError> {
    let bar = ProgressBar::no_length()
        .with_style(PROGRESS_STYLE_DELETE_SPINNER.clone())
        .with_message(format!("Removing {version}"));
    multi_progress.add(bar.clone());

    let progress = |status| match status {
        RemoveProgress::Start { total_bytes } => {
            bar.reset();
            bar.set_length(total_bytes);
            bar.set_style(PROGRESS_STYLE_DELETE.clone());
        }
        RemoveProgress::Progress { bytes_removed } => {
            bar.set_position(bytes_removed);
        }
        RemoveProgress::End => {
            bar.finish_with_message(format!("{version} is removed"));
        }
    };

    client.remove(&version, progress, &cancel_token).await?;

    Ok(bar.length().unwrap_or(0))
}
