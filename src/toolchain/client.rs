use std::{
    fmt::Debug,
    io::{ErrorKind, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use camino::Utf8Path;
use data_encoding::HEXLOWER;
use futures::{TryStreamExt, future::join_all};
use octocrab::{Octocrab, models::repos::Asset};
use reqwest::header;
use sha2::{Digest, Sha256};
use tokio::io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, BufWriter};
use tokio_util::{future::FutureExt as _, sync::CancellationToken};
use tracing::{debug, info, instrument, trace, warn};

use crate::{
    CheckCancellation, DIRS, TRASH, fs,
    toolchain::{
        APP_USER_AGENT, InstallState, InstalledToolchain, ToolchainError, ToolchainRelease,
        ToolchainVersion, extract,
        remove::{RemoveProgress, remove_dir_progress},
    },
};

/// A client for downloading and installing the Arm Toolchain for Embedded (ATfE).
#[derive(Clone)]
pub struct ToolchainClient {
    gh_client: Arc<Octocrab>,
    client: reqwest::Client,
    cache_path: PathBuf,
    toolchains_path: PathBuf,
    current_version: Arc<RwLock<Option<ToolchainVersion>>>,
}

impl Debug for ToolchainClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolchainClient")
            .field("cache_path", &self.cache_path)
            .field("toolchains_path", &self.toolchains_path)
            .finish()
    }
}

impl ToolchainClient {
    pub const REPO_OWNER: &str = "arm";
    pub const REPO_NAME: &str = "arm-toolchain";
    pub const RELEASE_PREFIX: &str = "release-";
    pub const RELEASE_SUFFIX: &str = "-ATfE"; // arm toolchain for embedded
    pub const CURRENT_TOOLCHAIN_FILENAME: &str = "current.txt";

    /// Creates a new toolchain client that installs to a platform-specific data directory.
    ///
    /// For example, on macOS this is
    /// `~/Library/Application Support/dev.vexide.arm-toolchain/llvm-toolchains`.
    pub async fn using_data_dir() -> Result<Self, ToolchainError> {
        Self::new(
            DIRS.data_local_dir().join("llvm-toolchains"),
            DIRS.cache_dir().join("downloads/llvm-toolchains"),
        )
        .await
    }

    /// Creates a client that installs toolchains in the specified folder.
    pub async fn new(
        toolchains_path: impl Into<PathBuf>,
        cache_path: impl Into<PathBuf>,
    ) -> Result<Self, ToolchainError> {
        let toolchains_path = toolchains_path.into();
        let cache_path = cache_path.into();
        trace!(
            ?toolchains_path,
            ?cache_path,
            "Initializing toolchain downloader"
        );

        let (current_version, setup_fut) = tokio::join!(
            fs::read_to_string(toolchains_path.join(Self::CURRENT_TOOLCHAIN_FILENAME)),
            async {
                tokio::try_join!(
                    fs::create_dir_all(&toolchains_path),
                    fs::create_dir_all(&cache_path),
                )
            },
        );

        setup_fut?;

        let current_version = current_version
            .map(|name| ToolchainVersion::named(name.trim()))
            .ok();

        Ok(Self {
            gh_client: octocrab::instance(),
            client: reqwest::Client::builder()
                .user_agent(APP_USER_AGENT)
                .build()
                .unwrap(),
            toolchains_path,
            cache_path,
            current_version: Arc::new(RwLock::new(current_version)),
        })
    }

    /// Fetches the latest release of the Arm Toolchain for Embedded (ATfE) from the ARM GitHub repository.
    #[instrument(skip(self))]
    pub async fn latest_release(&self) -> Result<ToolchainRelease, ToolchainError> {
        debug!("Fetching latest release from GitHub repo");

        let releases = self
            .gh_client
            .repos(Self::REPO_OWNER, Self::REPO_NAME)
            .releases()
            .list()
            .per_page(10)
            .send()
            .await?;

        let Some(latest_embedded_release) = releases
            .items
            .iter()
            .find(|r| r.tag_name.ends_with(Self::RELEASE_SUFFIX))
        else {
            return Err(ToolchainError::LatestReleaseMissing {
                candidates: releases.items.into_iter().map(|r| r.tag_name).collect(),
            });
        };

        Ok(ToolchainRelease::new(latest_embedded_release.clone()))
    }

    /// Fetches the given release of the Arm Toolchain for Embedded (ATfE) from the ARM GitHub repository.
    #[instrument(skip(self))]
    pub async fn get_release(
        &self,
        version: &ToolchainVersion,
    ) -> Result<ToolchainRelease, ToolchainError> {
        let tag_name = version.to_tag_name();
        info!(%tag_name, "Fetching release data from GitHub");

        let release = self
            .gh_client
            .repos(Self::REPO_OWNER, Self::REPO_NAME)
            .releases()
            .get_by_tag(&tag_name)
            .await?;

        Ok(ToolchainRelease::new(release.clone()))
    }

    /// Returns the path where the given toolchain version would be installed.
    pub fn install_path_for(&self, version: &ToolchainVersion) -> PathBuf {
        self.toolchains_path.join(&version.name)
    }

    /// Checks if the specified toolchain version is already installed.
    pub fn version_is_installed(&self, version: &ToolchainVersion) -> bool {
        self.install_path_for(version).exists()
    }

    /// Downloads the specified toolchain asset, verifies its checksum, extracts it,
    /// and installs it to the appropriate location.
    ///
    /// The downloaded toolchain will be activated if there is no other active toolchain. Returns
    /// the path to the extracted toolchain directory.
    ///
    /// # Resuming downloads
    ///
    /// This method will also handle resuming downloads if the file already exists and is partially downloaded.
    /// If the partially-downloaded file contains invalid bytes, a checksum error will be returned and the file
    /// will be deleted.
    #[instrument(
        skip(self, release, asset, progress, cancel_token),
        fields(version = release.version().name, asset.name)
    )]
    pub async fn download_and_install(
        &self,
        release: &ToolchainRelease,
        asset: &Asset,
        progress: Arc<dyn Fn(InstallState) + Send + Sync>,
        cancel_token: CancellationToken,
    ) -> Result<PathBuf, ToolchainError> {
        let file_name = Utf8Path::new(&asset.name).file_name().ok_or_else(|| {
            ToolchainError::InvalidAssetName {
                name: asset.name.to_string(),
            }
        })?;
        let archive_destination = self.cache_path.join(file_name);

        debug!(asset.name, ?archive_destination, "Downloading asset");

        // Begin downloading the checksum file in parallel so it's ready when we need it.
        let checksum_future = self.fetch_asset_checksum(asset);

        // Meanwhile, either begin or resume the asset download.
        let download_task = async {
            let mut downloaded_file = self
                .download_asset(asset, &archive_destination, progress.clone())
                .await?;

            debug!("Calculating checksum for downloaded file");
            let checksum_bytes =
                calculate_file_checksum(&mut downloaded_file, progress.clone()).await?;
            let checksum_hex = HEXLOWER.encode(&checksum_bytes);
            trace!(?checksum_hex, "Checksum calculated");

            Ok::<_, ToolchainError>((downloaded_file, checksum_hex))
        };

        let ((mut downloaded_file, real_checksum), expected_checksum) =
            async { tokio::try_join!(download_task, checksum_future) }
                .with_cancellation_token(&cancel_token)
                .await
                .ok_or(ToolchainError::Cancelled)??;

        // Verify the checksum to make sure the download was successful and the file is not corrupted.

        let checksums_match = real_checksum.eq_ignore_ascii_case(&expected_checksum);
        debug!(
            ?real_checksum,
            ?expected_checksum,
            "Checksum verification: {checksums_match}"
        );
        if !checksums_match {
            fs::remove_file(archive_destination).await?;
            return Err(ToolchainError::ChecksumMismatch {
                expected: expected_checksum,
                actual: real_checksum,
            });
        }

        debug!("Download finished");

        // Now choose the extraction method based on the file extension.

        let extract_location = self.install_path_for(release.version());

        cancel_token.check_cancellation(ToolchainError::Cancelled)?;

        debug!(archive = ?archive_destination, ?extract_location, "Extracting downloaded archive");
        progress(InstallState::ExtractBegin);

        if extract_location.exists() {
            debug!("Destination folder already exists, removing it");
            TRASH.delete(&extract_location)?;
        }

        downloaded_file.seek(SeekFrom::Start(0)).await?;
        if file_name.ends_with(".dmg") {
            extract::macos::extract_dmg(
                archive_destination.clone(),
                &extract_location,
                progress.clone(),
                cancel_token,
            )
            .await?;
        } else if file_name.ends_with(".zip") {
            extract::extract_zip(downloaded_file, extract_location.clone()).await?;
        } else if file_name.ends_with(".tar.xz") {
            let progress = progress.clone();
            extract::extract_tar_xz(
                downloaded_file,
                extract_location.clone(),
                progress.clone(),
                cancel_token,
            )
            .await?;
        } else {
            unreachable!("Unsupported file format");
        }

        progress(InstallState::ExtractCleanUp);
        fs::remove_file(archive_destination).await?;

        progress(InstallState::ExtractDone);

        debug!("Updating current toolchain if necessary.");
        if self.active_toolchain().is_none() {
            let new_version = release.version().clone();
            info!(%new_version, "Updating current toolchain");
            self.set_active_toolchain(Some(release.version().clone()))
                .await?;
        }

        Ok(extract_location)
    }

    /// Downloads the asset to the specified destination path without checksum verification or extraction.
    ///
    /// If the destination path already has a partially downloaded file, it will resume the download from where it left off.
    #[instrument(skip(self, asset, progress))]
    async fn download_asset(
        &self,
        asset: &Asset,
        destination: &Path,
        progress: Arc<dyn Fn(InstallState) + Send + Sync>,
    ) -> Result<fs::File, ToolchainError> {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).await?;
        }

        let mut file = fs::File::options()
            .read(true)
            .append(true)
            .create(true)
            .open(&destination)
            .await?;

        let mut current_file_length = file.seek(SeekFrom::End(0)).await?;

        // Some initial checks before we start downloading to see if it makes sense to continue.

        if current_file_length > asset.size as u64 {
            // Having *too much* data doesn't make any sense... just restart the download from scratch.
            warn!(
                ?current_file_length,
                ?asset.size,
                "File size mismatch: existing file is larger than expected. Truncating file and starting over."
            );

            file.set_len(0).await?;
            current_file_length = file.seek(SeekFrom::End(0)).await?;
        }

        if current_file_length == asset.size as u64 {
            debug!("File already downloaded, skipping download");
            return Ok(file);
        }

        // If there's already data in the file, we will assume that's from the last download attempt and
        // set the Range header to continue downloading from where we left off.

        let next_byte_index = current_file_length;
        let last_byte_index = asset.size as u64 - 1;
        let range_header = format!("bytes={next_byte_index}-{last_byte_index}");
        trace!(?range_header, "Setting Range header for download");

        if next_byte_index > 0 {
            debug!("Resuming an existing download");
        }

        progress(InstallState::DownloadBegin {
            asset_size: asset.size as u64,
            bytes_read: current_file_length,
        });

        // At this point, we're all good to just start copying bytes from the stream to the file.

        let mut stream = self
            .client
            .get(asset.browser_download_url.clone())
            .header(header::RANGE, range_header)
            .header(header::ACCEPT, "*/*")
            .send()
            .await?
            .error_for_status()?
            .bytes_stream();

        let mut writer = BufWriter::new(file);

        while let Some(chunk) = stream.try_next().await? {
            writer.write_all(&chunk).await?;

            current_file_length += chunk.len() as u64;
            progress(InstallState::Download {
                bytes_read: current_file_length,
            });
        }

        writer.flush().await?;
        progress(InstallState::DownloadFinish);
        debug!(?destination, "Download completed");

        Ok(writer.into_inner())
    }

    /// Downloads the expected SHA256 checksum for the asset.
    ///
    /// The resulting string contains the checksum in hex format.
    async fn fetch_asset_checksum(&self, asset: &Asset) -> Result<String, ToolchainError> {
        let mut sha256_url = asset.browser_download_url.clone();
        sha256_url.set_path(&format!("{}.sha256", sha256_url.path()));

        let mut checksum_file = self
            .client
            .get(sha256_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        // Trim off the filename from the checksum file, which is usually in the format:
        // `<checksum> <filename>`

        let mut parts = checksum_file.split_ascii_whitespace();
        let hash_part = parts.next().unwrap_or("");
        checksum_file.truncate(hash_part.len());

        Ok(checksum_file)
    }

    pub async fn installed_versions(&self) -> Result<Vec<ToolchainVersion>, ToolchainError> {
        let mut futs = vec![];

        let mut dir = fs::read_dir(&self.toolchains_path).await?;
        while let Some(entry) = dir.next_entry().await? {
            futs.push(async move {
                if let Ok(ty) = entry.file_type().await
                    && ty.is_dir()
                {
                    let name = entry.file_name();
                    return Some(ToolchainVersion::named(name.to_string_lossy()));
                }

                None
            });
        }

        let versions = join_all(futs).await.into_iter().flatten().collect();
        Ok(versions)
    }

    /// Delete all files related to the given toolchain version.
    pub async fn remove(
        &self,
        version: &ToolchainVersion,
        progress: impl FnMut(RemoveProgress),
        cancel_token: &CancellationToken,
    ) -> Result<(), ToolchainError> {
        if let Ok(toolchain) = self.toolchain(version).await {
            remove_dir_progress(toolchain.path, progress, cancel_token).await?;
        }

        if self.active_toolchain().as_ref() == Some(version) {
            self.set_active_toolchain(None).await?;
        }

        Ok(())
    }

    /// Delete the cache directory, returning the number of bytes deleted.
    pub async fn purge_cache(&self) -> Result<u64, ToolchainError> {
        let bytes = async {
            let mut bytes = 0;

            let mut read_dir = fs::read_dir(&self.cache_path).await?;
            while let Some(item) = read_dir.next_entry().await? {
                let meta = item.metadata().await?;
                bytes += meta.len();
            }

            Ok::<u64, ToolchainError>(bytes)
        };

        let bytes = bytes.await.unwrap_or(0);
        fs::remove_dir_all(&self.cache_path).await?;
        Ok(bytes)
    }

    /// Get the version of the active (default) toolchain.
    pub fn active_toolchain(&self) -> Option<ToolchainVersion> {
        self.current_version.read().unwrap().clone()
    }

    /// Set the version of the active (default) toolchain.
    ///
    /// This will write the given value to disk.
    pub async fn set_active_toolchain(
        &self,
        version: Option<ToolchainVersion>,
    ) -> Result<(), ToolchainError> {
        let path = self.toolchains_path.join(Self::CURRENT_TOOLCHAIN_FILENAME);

        if let Some(version) = &version {
            fs::write(path, &version.name).await?;
        } else {
            match fs::remove_file(path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                other => other,
            }?;
        }

        *self.current_version.write().unwrap() = version;

        Ok(())
    }

    /// Returns a struct used to access paths of an installed toolchain.
    ///
    /// This doesn't check whether the specified version is actually installed,
    /// so make sure the paths exist before using them.
    pub async fn toolchain(
        &self,
        version: &ToolchainVersion,
    ) -> Result<InstalledToolchain, ToolchainError> {
        let toolchain = InstalledToolchain::new(self.toolchains_path.join(&version.name));
        toolchain.check_installed().await?;
        Ok(toolchain)
    }
}

/// Scans an entire file and calculates its SHA256 checksum.
async fn calculate_file_checksum(
    file: &mut fs::File,
    progress: Arc<dyn Fn(InstallState) + Send + Sync>,
) -> Result<[u8; 32], io::Error> {
    let file_size = file.metadata().await?.len();
    progress(InstallState::VerifyingBegin {
        asset_size: file_size,
    });

    file.seek(SeekFrom::Start(0)).await?;
    let mut reader = BufReader::new(file);

    let mut hasher = Sha256::default();
    let mut data = vec![0; 64 * 1024];

    let mut bytes_read = 0;
    loop {
        let len = reader.read(&mut data).await?;
        if len == 0 {
            break;
        }

        hasher.update(&data[..len]);

        bytes_read += len as u64;
        progress(InstallState::Verifying { bytes_read });
    }

    let checksum = hasher.finalize().into();

    progress(InstallState::VerifyingFinish);

    Ok(checksum)
}
