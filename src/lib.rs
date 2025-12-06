//! # ARM Toolchain Manager
//!
//! This is the support library for the ARM Toolchain Manager CLI. It provides a client
//! for downloading, installing, using, and deleting versions of the open-source
//! ARM Toolchain for Embedded. This library's author is not affiliated with ARM.
//!
//! ## Toolchain module
//!
//! Use the [`toolchain`] module to access and modify ARM toolchains. Most operations
//! in this module require you to create a [`ToolchainClient`](toolchain::ToolchainClient).
//! From there, you can do things like getting a list of installed toolchains, getting their
//! `bin` paths, downloading a new toolchain, etc.
//!
//! ## CLI module
//!
//! (Cargo feature: `cli` [default])
//!
//! The [`cli`] module is for integrating this library into another CLI tool. It provides
//! specialized wrappers for the operations in the `toolchain` module that work well with
//! applications using [`clap`]. The functions in this module will print to stdio and read
//! user input.
//!
//! ## CLI Binaries
//!
//! (Cargo feature: `bin`)
//!
//! This library has two associated CLI tools, `arm-toolchain` and `atrun`. They have their
//! own dependencies, so to compile them you will need to enable a Cargo feature.
//!
//! ```sh
//! cargo install arm-toolchain -Fbin
//! ```

use std::sync::LazyLock;

use directories::ProjectDirs;

pub(crate) use fs_err::tokio as fs;
use tokio_util::sync::CancellationToken;
use trash::TrashContext;

pub mod cli;
pub mod toolchain;

pub static DIRS: LazyLock<ProjectDirs> = LazyLock::new(|| {
    ProjectDirs::from("dev", "vexide", "arm-toolchain").expect("home directory must be available")
});

pub static TRASH: LazyLock<TrashContext> = LazyLock::new(|| {
    #[allow(unused_mut)]
    let mut ctx = TrashContext::new();

    // Opt in to faster deletion method
    #[cfg(target_os = "macos")]
    trash::macos::TrashContextExtMacos::set_delete_method(
        &mut ctx,
        trash::macos::DeleteMethod::NsFileManager,
    );

    ctx
});

trait CheckCancellation {
    fn check_cancellation<E>(&self, error: E) -> Result<(), E>;
}

impl CheckCancellation for CancellationToken {
    fn check_cancellation<E>(&self, error: E) -> Result<(), E> {
        if self.is_cancelled() {
            Err(error)
        } else {
            Ok(())
        }
    }
}
