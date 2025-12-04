use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::toolchain::ToolchainError;
use crate::{CheckCancellation, fs};

pub enum RemoveProgress {
    Start { total_bytes: u64 },
    Progress { bytes_removed: u64 },
    End,
}

pub async fn remove_dir_progress(
    dir: PathBuf,
    mut progress: impl FnMut(RemoveProgress),
    cancel_token: &CancellationToken,
) -> Result<(), ToolchainError> {
    let mut items = vec![];
    let total_bytes = enumerate_dir(dir, &mut items, cancel_token).await?;
    let mut bytes_removed = 0;

    progress(RemoveProgress::Start { total_bytes });

    for item in items {
        if item.sym {
            if cfg!(windows) && item.dir {
                fs::remove_dir(&item.path).await?;
            } else {
                fs::remove_file(&item.path).await?;
            }
        } else if item.dir {
            fs::remove_dir(&item.path).await?;
        } else {
            fs::remove_file(&item.path).await?;
            bytes_removed += item.size;
        }

        progress(RemoveProgress::Progress { bytes_removed });
    }

    progress(RemoveProgress::End);

    Ok(())
}

async fn enumerate_dir(
    path: PathBuf,
    contents_vec: &mut Vec<Item>,
    cancel_token: &CancellationToken,
) -> Result<u64, ToolchainError> {
    let mut bytes = 0;

    let meta = fs::symlink_metadata(&path).await?;

    if meta.is_symlink() {
        contents_vec.push(Item {
            path,
            dir: meta.is_dir(),
            sym: true,
            size: meta.len(),
        });
        return Ok(meta.len());
    }

    if meta.is_file() {
        contents_vec.push(Item {
            path,
            dir: false,
            sym: false,
            size: meta.len(),
        });
        return Ok(meta.len());
    }

    let mut read_dir = fs::read_dir(&path).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        cancel_token.check_cancellation(ToolchainError::Cancelled)?;

        let path = entry.path();
        bytes += Box::pin(enumerate_dir(path, contents_vec, cancel_token)).await?;
    }

    contents_vec.push(Item {
        path,
        dir: true,
        sym: false,
        size: meta.len(),
    });

    Ok(bytes + meta.len())
}

struct Item {
    path: PathBuf,
    sym: bool,
    dir: bool,
    size: u64,
}
