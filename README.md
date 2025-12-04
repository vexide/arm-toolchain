# ARM Toolchain Manager

ARM Toolchain Manager is a tool for installing and managing the LLVM-based ARM embedded toolchain.

## Features

- `arm-toolchain` command: Utility for the downloading, installing, and managing of ARM toolchains
- `atrun` command: Run commands from the active toolchain without adding it to your `PATH`.
- Library mode: Query installed toolchains programmatically via the Rust crate.
- Cargo xtask integration: Add the tool as a subcommand of your Cargo project's xtask command.

## Usage

You can download and activate the latest toolchain with this command:

```shell
arm-toolchain use latest
```

Or pick a custom version:

```shell
arm-toolchain use v21.1.0
```

Once you've activated a toolchain, run commands from it with `atrun`:

```terminal
$ atrun clang -print-targets

  Registered Targets:
    aarch64    - AArch64 (little endian)
    aarch64_32 - AArch64 (little endian ILP32)
    aarch64_be - AArch64 (big endian)
    arm        - ARM
    arm64      - ARM64 (little endian)
    arm64_32   - ARM64 (little endian ILP32)
    armeb      - ARM (big endian)
    thumb      - Thumb
    thumbeb    - Thumb (big endian)
```

### Locate toolchains

Use the `locate` subcommand to get the path to the active toolchain, or a specified one.

```shell
arm-toolchain locate
export PATH="$(arm-toolchain locate bin):$PATH"

arm-toolchain locate -T v21.1.0
export PATH="$(arm-toolchain locate bin -T v21.1.0):$PATH"
```

If you are collaborating with others, you might want to make a symlink to the toolchain so that you can refer to its path without hardcoding anything too unpredictable. Here's how you'd do that:

```shell
echo "/toolchain" >> .gitignore
ln -s "$(arm-toolchain locate)" toolchain
```

After this initial setup, you can use consistent paths in other places.

```shell
./toolchain/bin/clang -print-targets
export C_INCLUDE_PATH="toolchain/lib/clang-runtimes/arm-none-eabi/include"
```

### List toolchains

You can view all the installed toolchains with the `list` subcommand.

```terminal
$ arm-toolchain list
Active: v21.1.1

Installed:
- v21.1.1
- v21.1.0
```

### Remove toolchains

You can remove toolchains when you're done using them.

```shell
arm-toolchain remove v21.1.0
arm-toolchain remove all
```

You can also purge the download cache to save space. `arm-toolchain` will delete things from the cache after it finishes downloading them, but if it gets interrupted you might end up with some excess files in there.

```shell
arm-toolchain purge-cache
```

### Integration with cargo xtask

If your Rust project uses the [xtask pattern](https://github.com/matklad/cargo-xtask), you can make `arm-toolchain` a subcommand by adding it to your existing parser.

```toml
[dependencies]
arm_toolchain = { version = "*", features = ["cli"] }
```

```rs
use arm_toolchain::cli::ArmToolchainCmd;

#[derive(Debug, clap::Parser)]
enum Args {
    // ...
    Toolchain(ArmToolchainCmd),
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let args = Args::parse();

    match args {
        // ...
        Args::Toolchain(cmd) => {
            cmd.run().await?;
        }
    }
}
```

Now you can use the tool without having to install the standalone command.

```shell
cargo xtask toolchain use latest
```

### Library mode

All of this tool's functionality is exposed programmatically for your custom scripting needs. To access it, your project will need to be using the Tokio runtime.

```rs
let client = ToolchainClient::using_data_dir().await?;

let installed = client.installed_versions().await?;

println!("Installed toolchains:");
for version in installed {
    let toolchain = client.toolchain(&version);
    println!("- {version} at {}", toolchain.path.display());
}
```
