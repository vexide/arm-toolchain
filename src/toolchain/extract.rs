//! This module provides functionality to extract toolchain archives in formats
//! such as DMG, ZIP, and TAR.XZ.

use std::{
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
};

use fs_extra::dir::{CopyOptions, TransitProcess, TransitProcessResult, copy_with_progress};
use liblzma::read::XzDecoder;
use miette::Diagnostic;
use tempfile::tempdir;
use thiserror::Error;
use tokio::{io, task::spawn_blocking};
use tokio_util::sync::CancellationToken;
use tracing::debug;
use zip::{read::root_dir_common_filter, result::ZipError};

use crate::{CheckCancellation, fs, toolchain::ToolchainError};

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(not(target_os = "macos"))]
pub mod macos {
    use indicatif::ProgressBar;
    use tokio_util::sync::CancellationToken;

    use super::*;

    pub async fn extract_dmg(
        _dmg_path: PathBuf,
        _destination_folder: &Path,
        _cancel_token: CancellationToken,
    ) -> Result<(), ToolchainError> {
        Err(ExtractError::DmgNotSupported.into())
    }
}

#[derive(Debug, Error, Diagnostic)]
pub enum ExtractError {
    #[error("DMG extraction is not supported on this platform")]
    #[diagnostic(code(arm_toolchain::extract::dmg_not_supported))]
    DmgNotSupported,

    #[error("The archive did not contain the expected contents")]
    #[diagnostic(code(arm_toolchain::extract::contents_not_found))]
    ContentsNotFound,

    #[error("Failed to read directory while extracting toolchain")]
    #[diagnostic(code(arm_toolchain::extract::dir_copy_failed))]
    DirCopy(#[from] fs_extra::error::Error),

    #[error("DMG extraction failed")]
    #[diagnostic(code(arm_toolchain::extract::dmg_failed))]
    Dmg(#[source] io::Error),

    #[error("ZIP extraction failed")]
    #[diagnostic(code(arm_toolchain::extract::zip_failed))]
    Zip(#[from] ZipError),
}

pub async fn extract_zip(
    zip_file: fs::File,
    destination: PathBuf,
) -> Result<fs::File, ToolchainError> {
    let mut reader = BufReader::new(zip_file.into_std().await);

    let file = spawn_blocking(move || {
        let mut archive = zip::ZipArchive::new(&mut reader)?;

        archive.extract_unwrapped_root_dir(destination, root_dir_common_filter)?;

        Ok::<_, ZipError>(reader.into_inner())
    })
    .await
    .unwrap()
    .map_err(ExtractError::Zip)?;

    Ok(file.into())
}

pub async fn extract_tar_xz(
    tar_xz_file: fs::File,
    destination: PathBuf,
    progress: impl FnMut(TransitProcess) + Send + 'static,
    cancel_token: CancellationToken,
) -> Result<fs::File, ToolchainError> {
    let mut reader = BufReader::new(tar_xz_file.into_std().await);

    let temp_destination = Arc::new(tempdir()?);

    // This behavior is necessary because the archive contains a sub-directory which we want to ignore.
    debug!(
        temp_dir = ?temp_destination.path(),
        "This tar.xz archive will be extracted to a temporary directory before being moved to the final destination"
    );

    let file = spawn_blocking({
        let temp_destination = temp_destination.clone();
        move || {
            let mut decompressor = XzDecoder::new(&mut reader);
            let mut archive = tar::Archive::new(&mut decompressor);

            archive.unpack(temp_destination.path())?;
            debug!("Done unpacking");
            Ok::<_, io::Error>(reader.into_inner())
        }
    })
    .await
    .unwrap()?;

    // Find the root directory in the extracted contents and move it to the destination
    let root_dir = find_dir_contained_by(temp_destination.path()).await?;
    debug!("mv");
    mv(&root_dir, &destination, progress, cancel_token).await?;

    Ok(file.into())
}

async fn find_dir_contained_by(parent_dir: &Path) -> Result<PathBuf, ToolchainError> {
    let mut contents_path = None;

    let mut read_dir = fs::read_dir(parent_dir).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let metadata = entry.metadata().await?;
        let is_dir = metadata.is_dir() && !metadata.is_symlink();
        if is_dir {
            contents_path = Some(entry.path());
            break;
        }
    }

    Ok(contents_path.ok_or(ExtractError::ContentsNotFound)?)
}

pub async fn mv(
    src: &Path,
    dst: &Path,
    progress: impl FnMut(TransitProcess) + Send + 'static,
    cancel_token: CancellationToken,
) -> Result<(), ToolchainError> {
    match fs::rename(src, dst).await {
        Ok(()) => Ok(()),
        // Moving from /tmp/ to /anywhere-else/ isn't possible with a simple fs::rename because
        // we're moving across devices, so we'll fallback to the more complicated recursive
        // copy-and-delete method if that fails.
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_folder(
                src.to_path_buf(),
                dst.to_path_buf(),
                progress,
                cancel_token.clone(),
            )
            .await?;
            Ok(())
        }
        Err(e) => Err(ToolchainError::Io(e)),
    }
}

async fn copy_folder(
    source: PathBuf,
    destination: PathBuf,
    mut progress: impl FnMut(TransitProcess) + Send + 'static,
    cancel_token: CancellationToken,
) -> Result<(), ToolchainError> {
    debug!("Copying folder");

    fs::create_dir_all(&destination).await?;

    let tok = cancel_token.clone();
    let task = spawn_blocking(move || {
        let options = CopyOptions::new()
            .copy_inside(true);
        let handle = move |process_info: TransitProcess| {
            progress(process_info);
            if tok.is_cancelled() {
                return TransitProcessResult::Abort;
            }

            TransitProcessResult::ContinueOrAbort
        };

        copy_with_progress(source, destination, &options, handle)
    });

    let outcome = task.await.unwrap();

    cancel_token.check_cancellation(ToolchainError::Cancelled)?;
    outcome.map_err(ExtractError::from)?;

    Ok(())
}
