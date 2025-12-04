//! This module provides functionality to extract toolchain archives in formats
//! such as DMG, ZIP, and TAR.XZ.

use std::{
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::future::try_join_all;
use liblzma::read::XzDecoder;
use miette::Diagnostic;
use tempfile::tempdir;
use thiserror::Error;
use tokio::{io, task::spawn_blocking};
use tokio_util::sync::CancellationToken;
use tracing::debug;
use zip::{read::root_dir_common_filter, result::ZipError};

use crate::{
    CheckCancellation, fs,
    toolchain::{InstallState, ToolchainError},
};

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
        _progress: Arc<dyn Fn(InstallState) + Send + Sync>,
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
    progress: Arc<dyn Fn(InstallState) + Send + Sync>,
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
    progress: Arc<dyn Fn(InstallState) + Send + Sync>,
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
    progress: Arc<dyn Fn(InstallState) + Send + Sync>,
    cancel_token: CancellationToken,
) -> Result<(), ToolchainError> {
    debug!("Copying folder");

    fs::create_dir_all(&destination).await?;

    // First enumerate files from the source & create destination directories.
    let mut files = vec![];
    let total_size = create_scaffolding(&source, &destination, &mut files, &cancel_token).await?;
    let mut bytes_so_far = 0;

    for (size, source_path, sym_type) in files {
        let inner_path = Path::new(&source_path)
            .strip_prefix(&source)
            .expect("subdir path should have prefix of source directory");
        let new_path = destination.join(inner_path);

        if let Some(ty) = sym_type {
            let ptr = fs::read_link(source_path).await?;

            if ty == SymType::File {
                #[cfg(unix)]
                fs::symlink(ptr, &new_path).await?;
                #[cfg(windows)]
                fs::symlink_file(ptr, &new_path).await?;
            } else {
                #[cfg(unix)]
                fs::symlink(ptr, &new_path).await?;
                #[cfg(windows)]
                fs::symlink_dir(ptr, &new_path).await?;
            }

            // fs::set_permissions(new_path, perms).await?;
        } else {
            fs::copy(source_path, new_path).await?;
            bytes_so_far += size;

            progress(InstallState::ExtractCopy {
                total_size,
                bytes_copied: bytes_so_far,
            })
        }
    }

    Ok(())
}

async fn create_scaffolding(
    source: &Path,
    destination: &Path,
    files_vec: &mut Vec<(u64, PathBuf, Option<SymType>)>,
    cancel_token: &CancellationToken,
) -> Result<u64, ToolchainError> {
    let mut bytes = 0;

    let mut sub_dirs = vec![];
    let mut mkdir_tasks = vec![];

    let mut read_dir = fs::read_dir(source).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        cancel_token.check_cancellation(ToolchainError::Cancelled)?;

        let name = entry.file_name();
        let path = entry.path();
        let meta = entry.metadata().await?;

        if meta.is_symlink() {
            let ty = if meta.is_dir() {
                SymType::Dir
            } else {
                SymType::File
            };

            files_vec.push((0, path, Some(ty)));
            continue;
        }

        if meta.is_dir() {
            sub_dirs.push(name.clone());
            mkdir_tasks.push(async move {
                let inner_path = Path::new(&path)
                    .strip_prefix(source)
                    .expect("subdir path should have prefix of source directory");
                let new_path = destination.join(inner_path);

                fs::create_dir(&new_path).await?;
                fs::set_permissions(&new_path, meta.permissions()).await?;

                Ok::<(), io::Error>(())
            });
        } else {
            files_vec.push((meta.len(), path, None));
            bytes += meta.len();
        }
    }

    try_join_all(mkdir_tasks).await?;

    for name in &sub_dirs {
        bytes += Box::pin(create_scaffolding(
            &source.join(name),
            &destination.join(name),
            files_vec,
            cancel_token,
        ))
        .await?;
    }

    Ok(bytes)
}

#[derive(Debug, PartialEq)]
enum SymType {
    File,
    Dir,
}
