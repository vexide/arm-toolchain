use std::{cell::OnceCell, env, io::ErrorKind, path::PathBuf, process::Command, str::FromStr};

use serde::Deserialize;
use tracing::{debug, trace};

use crate::{
    Error,
    build::{BuildError, BuildTarget},
    fs,
};

#[derive(Debug)]
pub struct Project {
    path: PathBuf,
    config: OnceCell<ProjectConfig>,
}

impl Project {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            config: OnceCell::new(),
        }
    }

    pub async fn find() -> Result<Self> {
        let mut candidate = env::current_dir()?;
        loop {
            trace!(?candidate, "Searching for project root (Package.swift)");

            let mut read_dir = fs::read_dir(&candidate).await?;
            while let Some(entry) = read_dir.next_entry().await? {
                if entry.file_name().eq_ignore_ascii_case("Package.swift") {
                    debug!(path = ?candidate, "Found project root");
                    return Ok(Self::new(candidate));
                }
            }

            if let Some(parent) = candidate.parent() {
                candidate = parent.to_owned();
            } else {
                return Err(Error::CannotFindProject);
            }
        }
    }

    pub fn config_path(&self) -> PathBuf {
        self.path.join(ProjectConfig::FILE_NAME)
    }

    pub fn output_path(target: &BuildTarget) -> crate::Result<PathBuf> {
        let path = Command::new("swift")
            .arg("build")
            .arg("-c")
            .arg(target.arg())
            .arg("--triple")
            .arg("armv7-none-none-eabi")
            .arg("--show-bin-path")
            .output()?;
        let path = PathBuf::from_str(
            String::from_utf8(path.stdout)
                .map_err(|_| BuildError::OutputFolderInvalid)?
                .trim(),
        )
        .map_err(|_| BuildError::OutputFolderInvalid)?;
        Ok(path)
    }
    pub fn executable_name() -> crate::Result<String> {
        let name = Command::new("swift")
            .arg("package")
            .arg("show-executables")
            .output()?;
        let name = String::from_utf8(name.stdout).map_err(|_| BuildError::ExecutableNameInvalid)?;
        let name = name
            .lines()
            .next()
            .ok_or(BuildError::ExecutableNameInvalid)?;
        Ok(name.to_string())
    }

    pub async fn config(&self) -> Result<Option<&ProjectConfig>> {
        if let Some(config) = self.config.get() {
            return Ok(Some(config));
        }

        let config_path = self.config_path();
        debug!(?config_path, "Attempting to read config");

        match fs::read_to_string(config_path).await {
            Ok(contents) => {
                let parsed: ProjectConfig = contents.parse()?;
                self.config.set(parsed).unwrap();
                Ok(self.config.get())
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                debug!("No config file found");
                Ok(None)
            }
            Err(e) => Err(Error::from(e)),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct ProjectConfig {
    pub llvm_version: String,
}

impl ProjectConfig {
    const FILE_NAME: &str = "llvm-toolchain.toml";
}

impl FromStr for ProjectConfig {
    type Err = toml::de::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        toml::from_str(s)
    }
}
