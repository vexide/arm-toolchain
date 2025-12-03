//! This module provides functionality to download and install the Arm Toolchain for Embedded (ATfE).
//!
//! The included [`ToolchainClient`] can be used to fetch the latest release from the Arm GitHub repository,
//! download the appropriate asset for the current host OS and architecture, and install it to a specified
//! directory. It also handles checksum verification and extraction of the downloaded archive.

use std::{
    cell::OnceCell,
    fmt::{self, Debug, Display},
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::Arc,
};

use camino::Utf8Path;
use data_encoding::HEXLOWER;
use futures::TryStreamExt;
use miette::Diagnostic;
use octocrab::{
    Octocrab,
    models::repos::{Asset, Release},
};
use reqwest::header;
use sha2::{Digest, Sha256};
use strum::AsRefStr;
use thiserror::Error;
use tokio::io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, BufWriter};
use tokio_util::{future::FutureExt as _, sync::CancellationToken};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::{CheckCancellation, DIRS, TRASH, fs};

mod extract;

static APP_USER_AGENT: &str = concat!(
    "vexide/",
    env!("CARGO_PKG_NAME"),
    "@",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CARGO_PKG_REPOSITORY"),
    ")",
);

#[derive(Debug, Error, Diagnostic)]
pub enum ToolchainError {
    #[error(
        "Failed to determine the latest Arm Toolchain for Embedded version.\nCandidates:\n{}",
        candidates.iter().map(|release| format!(" • {release}")).collect::<Vec<_>>().join("\n")
    )]
    #[diagnostic(code(arm_toolchain::toolchain::latest_release_not_found))]
    LatestReleaseMissing { candidates: Vec<String> },
    #[error(
        "Failed to determine a compatible toolchain asset for {allowed_os:?} {}.\nCandidates:\n{}",
        allowed_arches.iter().map(|a| a.as_ref()).collect::<Vec<_>>().join("/"),
        candidates.iter().map(|release| format!(" • {release}")).collect::<Vec<_>>().join("\n")
    )]
    #[diagnostic(code(arm_toolchain::toolchain::release_asset_not_found))]
    ReleaseAssetMissing {
        allowed_os: HostOS,
        allowed_arches: Vec<HostArch>,
        candidates: Vec<String>,
    },
    #[error("Cannot download {name} because it has an invalid name")]
    #[diagnostic(code(arm_toolchain::toolchain::invalid_asset_name))]
    InvalidAssetName { name: String },

    #[error(
        "The checksum of the downloaded asset did not match the expected value.
- Expected: {expected:?}
- Actual: {actual:?}"
    )]
    #[diagnostic(code(arm_toolchain::toolchain::checksum_mismatch))]
    #[diagnostic(help("the downloaded file may be corrupted or incomplete"))]
    ChecksumMismatch { expected: String, actual: String },

    #[error("Could not extract the toolchain asset")]
    #[diagnostic(transparent)]
    Extract(#[from] extract::ExtractError),

    #[error("The toolchain installation was cancelled")]
    #[diagnostic(code(arm_toolchain::toolchain::cancelled))]
    Cancelled,

    #[error("A request to the GitHub API failed")]
    #[diagnostic(code(arm_toolchain::toolchain::github_api))]
    GitHubApi(#[from] octocrab::Error),
    #[error("Failed to download the toolchain asset")]
    #[diagnostic(code(arm_toolchain::toolchain::download_failed))]
    Reqwest(#[from] reqwest::Error),
    #[error("Failed to move a file to the trash")]
    #[diagnostic(code(arm_toolchain::toolchain::trash_op_failed))]
    Trash(#[from] trash::Error),
    #[error(transparent)]
    #[diagnostic(code(arm_toolchain::toolchain::io_error))]
    Io(#[from] std::io::Error),
}

pub enum InstallState {
    DownloadBegin { asset_size: u64, bytes_read: u64 },
    Download { bytes_read: u64 },
    DownloadFinish,

    VerifyingBegin { asset_size: u64 },
    Verifying { bytes_read: u64 },
    VerifyingFinish,

    ExtractBegin,
    ExtractCopy(fs_extra::dir::TransitProcess),
    ExtractCleanUp,
    ExtractDone,
}

#[derive(Debug, AsRefStr, Clone, Copy)]
pub enum HostOS {
    Darwin,
    Linux,
    Windows,
}

impl HostOS {
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Darwin
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(windows) {
            Self::Windows
        } else {
            panic!("This OS is not supported by the ARM toolchain")
        }
    }
}

#[derive(Debug, AsRefStr, Clone, Copy)]
pub enum HostArch {
    #[strum(serialize = "universal")]
    Universal,
    AAarch64,
    #[strum(serialize = "x86_64")]
    X86_64,
}

impl HostArch {
    pub const fn current() -> &'static [Self] {
        const ALLOWED_ARCHES: &[HostArch] = &[
            #[cfg(target_arch = "x86_64")]
            HostArch::X86_64,
            #[cfg(target_arch = "aarch64")]
            HostArch::AAarch64,
            #[cfg(all(
                target_os = "macos",
                any(target_arch = "aarch64", target_arch = "x86_64")
            ))]
            HostArch::Universal,
        ];

        #[allow(clippy::const_is_empty)]
        if ALLOWED_ARCHES.is_empty() {
            panic!("This architecture is not supported by the ARM toolchain");
        }

        ALLOWED_ARCHES
    }
}

pub struct ToolchainRelease {
    release: Arc<Release>,
    version: OnceCell<ToolchainVersion>,
}

impl ToolchainRelease {
    const ALLOWED_EXTENSIONS: &[&str] = &["dmg", "tar.xz", "zip"];

    pub fn new(release: Release) -> Self {
        Self {
            version: OnceCell::new(),
            release: Arc::new(release),
        }
    }

    pub fn version(&self) -> &ToolchainVersion {
        self.version
            .get_or_init(|| ToolchainVersion::from_tag_name(&self.release.tag_name))
    }

    pub fn asset_for(
        &self,
        os: HostOS,
        allowed_arches: &[HostArch],
    ) -> Result<&Asset, ToolchainError> {
        debug!(
            options = self.release.assets.len(),
            ?os, ?allowed_arches, allowed_exts = ?Self::ALLOWED_EXTENSIONS,
            "Searching for a compatible toolchain asset"
        );

        let asset = self
            .release
            .assets
            .iter()
            .find(|a| {
                let mut components: Vec<&str> = a.name.split('-').collect();

                // Remove the file extension from the last file name component
                let last_idx = components.len() - 1;

                let (last_component, file_extension) = components[last_idx]
                    .split_once('.')
                    .expect("filename has extension");
                components[last_idx] = last_component;

                let correct_os = components.contains(&os.as_ref());
                let correct_arch = allowed_arches
                    .iter()
                    .any(|arch| components.contains(&arch.as_ref()));
                let correct_extension = Self::ALLOWED_EXTENSIONS.contains(&file_extension);

                let valid = correct_os && correct_arch && correct_extension;
                trace!(
                    name = a.name,
                    correct_os, correct_arch, correct_extension, "Asset valid: {valid}"
                );

                valid
            })
            .ok_or_else(|| ToolchainError::ReleaseAssetMissing {
                allowed_os: os,
                allowed_arches: allowed_arches.to_vec(),
                candidates: self
                    .release
                    .assets
                    .iter()
                    .map(|a| a.name.to_string())
                    .collect(),
            })?;

        debug!(name = asset.name, "Found compatible asset");

        Ok(asset)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainVersion {
    pub name: String,
}

impl ToolchainVersion {
    pub fn named(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub fn from_tag_name(tag_name: impl AsRef<str>) -> Self {
        let mut name = tag_name.as_ref();
        name = name
            .strip_prefix(ToolchainClient::RELEASE_PREFIX)
            .unwrap_or(name);
        name = name
            .strip_suffix(ToolchainClient::RELEASE_SUFFIX)
            .unwrap_or(name);

        Self {
            name: name.to_string(),
        }
    }

    fn to_tag_name(&self) -> String {
        format!(
            "{}{}{}",
            ToolchainClient::RELEASE_PREFIX,
            self.name,
            ToolchainClient::RELEASE_SUFFIX
        )
    }
}

impl Display for ToolchainVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.name)
    }
}

impl From<String> for ToolchainVersion {
    fn from(value: String) -> Self {
        Self::named(value)
    }
}

/// A client for downloading and installing the Arm Toolchain for Embedded (ATfE).
#[derive(Clone)]
pub struct ToolchainClient {
    gh_client: Arc<Octocrab>,
    client: reqwest::Client,
    cache_path: PathBuf,
    toolchains_path: PathBuf,
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

    /// Creates a new toolchain client that installs to a platform-specific data directory.
    ///
    /// For example, on macOS this is `~/Library/Application Support/dev.vexide.swift-v5/llvm-toolchains`.
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

        tokio::try_join!(
            fs::create_dir_all(&toolchains_path),
            fs::create_dir_all(&cache_path),
        )?;

        Ok(Self {
            gh_client: octocrab::instance(),
            client: reqwest::Client::builder()
                .user_agent(APP_USER_AGENT)
                .build()
                .unwrap(),
            toolchains_path,
            cache_path,
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

    /// Downloads the specified asset, verifies its checksum, extracts it, and installs it to the appropriate location.
    ///
    /// Returns the path to the extracted toolchain directory.
    ///
    /// This method will also handle resuming downloads if the file already exists and is partially downloaded.
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
                move |process| progress(InstallState::ExtractCopy(process)),
                cancel_token,
            )
            .await?;
        } else {
            unreachable!("Unsupported file format");
        }

        progress(InstallState::ExtractDone);

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
}

/// Scans the entire file and calculates its SHA256 checksum.
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
