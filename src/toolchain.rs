//! This module provides functionality to download and install the Arm Toolchain for Embedded (ATfE).
//!
//! The included [`ToolchainClient`] can be used to fetch the latest release from the Arm GitHub repository,
//! download the appropriate asset for the current host OS and architecture, and install it to a specified
//! directory. It also handles checksum verification and extraction of the downloaded archive.

use std::{
    cell::OnceCell,
    fmt::{self, Debug, Display},
    path::PathBuf,
    sync::Arc,
};

use miette::Diagnostic;
use octocrab::models::repos::{Asset, Release};
use strum::AsRefStr;
use thiserror::Error;
use tracing::{debug, error, trace};

mod client;
mod extract;
mod remove;

pub use client::*;
pub use remove::RemoveProgress;

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

    #[error("The toolchain {:?} is not installed.", version.name)]
    #[diagnostic(code(arm_toolchain::toolchain::not_installed))]
    ToolchainNotInstalled { version: ToolchainVersion },

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
    ExtractCopy { total_size: u64, bytes_copied: u64 },
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

impl From<&str> for ToolchainVersion {
    fn from(mut version: &str) -> Self {
        if let Some(bare) = version.strip_prefix("v") {
            version = bare;
        }

        ToolchainVersion::named(version)
    }
}

/// An ARM toolchain that may be installed on the current system.
pub struct InstalledToolchain {
    pub path: PathBuf,
}

impl InstalledToolchain {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub async fn check_installed(&self) -> Result<(), ToolchainError> {
        if !self.path.exists() {
            return Err(ToolchainError::ToolchainNotInstalled {
                version: ToolchainVersion::named(
                    self.path.file_name().unwrap_or_default().to_string_lossy(),
                ),
            });
        }

        Ok(())
    }

    /// Returns the path to a directory containing binaries that run on the host.
    ///
    /// This directory typically contains the compiler (`clang`) and support executables
    /// like `llvm-objcopy`.
    pub fn host_bin_dir(&self) -> PathBuf {
        self.path.join("bin")
    }

    /// Returns the path to a directory containing support libraries.
    ///
    /// This directory typically contains `libLTO.dylib`.
    pub fn lib_dir(&self) -> PathBuf {
        self.path.join("lib")
    }

    /// Returns the path to a directory containing a multilib.
    ///
    /// The path returned is equivalent to `self.lib_dir().join("clang-runtimes")`.
    ///
    /// This directory contains the libraries for all supported targets as well as a
    /// `multilib.yaml` file which describes which sub-directories they are located in.
    pub fn multilib_dir(&self) -> PathBuf {
        self.path.join("lib").join("clang-runtimes")
    }

    /// Returns the path to a directory containing static libraries for the given target.
    ///
    /// Targets are considered to have both a triple and a variant. Non-library files
    /// such as linker scripts or objects may be included in this directory.
    ///
    /// Example triples:
    ///
    /// - `arm-none-eabi`
    /// - `aarch64-none-elf`
    ///
    /// Example variants:
    ///
    /// - `armv7a_soft_nofp` (ARMv7-A, soft float ABI, no FPU)
    /// - `armv7m_soft_vfpv3_d16_exn_rtti` (ARMv7-M, soft float ABI, vfpv3 FPU, 16 float registers, with RTTI)
    /// - `armv7a_soft_vfpv3_d16_exn_rtti` (ARMv7-M, soft float ABI, vfpv3 FPU, 16 float registers, with RTTI)
    pub fn target_lib_dir(&self, triple: &str, variant: &str) -> PathBuf {
        self.multilib_dir().join(triple).join(variant).join("lib")
    }

    /// Returns the paths to header directories for the given target.
    ///
    /// Targets are considered to have both a triple and a variant.
    /// See [`Self::target_lib_dir`] for example triples and variants.
    pub fn target_include_dirs(&self, triple: &str, variant: &str) -> Vec<PathBuf> {
        let triple_dir = self.multilib_dir().join(triple);

        vec![
            triple_dir.join(variant).join("include"),
            triple_dir.join("include"),
        ]
    }
}
