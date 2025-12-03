//! Logic for extracting macOS DMG files.

use std::{
    mem,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dmg::detach;
use tokio::{task::spawn_blocking, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::{
    CheckCancellation,
    toolchain::{
        InstallState, ToolchainError,
        extract::{ExtractError, copy_folder, find_dir_contained_by},
    },
};

pub async fn extract_dmg(
    dmg_path: PathBuf,
    destination_folder: &Path,
    progress: Arc<dyn Fn(InstallState) + Send + Sync>,
    cancel_token: CancellationToken,
) -> Result<(), ToolchainError> {
    use dmg::Attach;
    debug!(?dmg_path, "Now mounting DMG");

    let handle = spawn_blocking(|| Attach::new(dmg_path).attach())
        .await
        .unwrap()
        .map_err(ExtractError::Dmg)?;

    let dmg = scopeguard::guard(handle, |handle| {
        // ensure the mount point is unmounted when we exit
        handle.force_detach().expect("Failed to detach DMG");
    });

    debug!(?dmg.mount_point, "Mounted DMG at temp path");

    // First directory in the mount point is the actual contents

    cancel_token.check_cancellation(ToolchainError::Cancelled)?;
    let contents_path = find_dir_contained_by(&dmg.mount_point).await?;

    info!(
        ?contents_path,
        ?destination_folder,
        "Extracting contents of DMG"
    );

    cancel_token.check_cancellation(ToolchainError::Cancelled)?;
    copy_folder(
        contents_path,
        destination_folder.to_owned(),
        {
            let progress = progress.clone();
            move |state| progress(InstallState::ExtractCopy(state))
        },
        cancel_token.clone(),
    )
    .await?;

    debug!(?dmg.mount_point, "Unmounting DMG");
    progress(InstallState::ExtractCleanUp);

    let mut retries_left = 10;
    while retries_left > 0 {
        cancel_token.check_cancellation(ToolchainError::Cancelled)?;
        retries_left -= 1;

        // Attempt to cleanly unmount the DMG instead of force detaching it.
        // This helps ensure everything is flushed properly.

        match detach(&dmg.device, false) {
            Ok(_) => {
                // No need to force unmount, we can safely abort the deferred cleanup
                mem::forget(dmg);
                break;
            }
            Err(error) => {
                debug!(?error, "Failed to unmount DMG, retrying...");
                sleep(Duration::from_millis(500)).await;
            }
        }
    }

    Ok(())
}
