use crate::{
    cli::{CliError, confirm_install, ctrl_c_cancel, install_with_progress_bar, msg},
    toolchain::{ToolchainClient, ToolchainVersion},
};

#[derive(Debug, clap::Parser)]
pub struct UseArgs {
    /// Version of LLVM to install
    pub llvm_version: ToolchainVersion,
}

pub async fn use_cmd(args: UseArgs) -> Result<(), CliError> {
    let mut version = args.llvm_version;

    let client = ToolchainClient::using_data_dir().await?;

    let install_latest = version.name == "latest";
    let mut release = None;

    // If "latest" specified we have to figure out what that actually means first
    if install_latest {
        let latest = client.latest_release().await?;
        version = latest.version().clone();
        release = Some(latest);
    }

    let installed_versions = client.installed_versions().await?;
    let is_installed = installed_versions.contains(&version);

    if !is_installed {
        let release = if let Some(rel) = release {
            rel
        } else {
            client.get_release(&version).await?
        };

        confirm_install(&version, install_latest).await?;

        let token = ctrl_c_cancel();
        install_with_progress_bar(&client, &release, token.clone()).await?;

        // Release Ctrl-C listener
        token.cancel();
    } else if client.active_toolchain().as_ref() == Some(&version) {
        println!("Toolchain {version} is already enabled.");
        return Ok(());
    }

    client.set_active_toolchain(Some(version.clone())).await?;

    msg!("Activated", "{version}");

    Ok(())
}
