use std::sync::LazyLock;

use directories::ProjectDirs;

pub(crate) use fs_err::tokio as fs;
use tokio_util::sync::CancellationToken;
use trash::TrashContext;

// pub mod project;
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
