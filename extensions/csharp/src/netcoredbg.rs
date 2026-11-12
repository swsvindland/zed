use std::fs;

use zed_extension_api::{self as zed, Result};

use crate::{
    dap_config,
    roslyn::{absolute_path, make_executable, newest_installed_version, remove_other_versions},
};

const REPOSITORY: &str = "Samsung/netcoredbg";

#[derive(Default)]
pub struct Netcoredbg {
    cached_path: Option<String>,
}

impl Netcoredbg {
    pub const ADAPTER_NAME: &'static str = "netcoredbg";

    pub fn binary(
        &mut self,
        config: zed::DebugTaskDefinition,
        user_provided_path: Option<String>,
        worktree: &zed::Worktree,
    ) -> Result<zed::DebugAdapterBinary> {
        let request = dap_config::parse(Self::ADAPTER_NAME, &config)?;

        let command = match user_provided_path.or_else(|| worktree.which(Self::ADAPTER_NAME)) {
            Some(path) => path,
            None => self.install()?,
        };

        Ok(zed::DebugAdapterBinary {
            command: Some(command),
            arguments: vec!["--interpreter=vscode".to_string()],
            envs: Vec::new(),
            cwd: Some(worktree.root_path()),
            connection: None,
            request_args: zed::StartDebuggingRequestArguments {
                configuration: config.config,
                request,
            },
        })
    }

    fn install(&mut self) -> Result<String> {
        if let Some(path) = &self.cached_path
            && fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
        {
            return Ok(path.clone());
        }

        let (asset_name, file_type) = match zed::current_platform() {
            (zed::Os::Windows, _) => ("netcoredbg-win64.zip", zed::DownloadedFileType::Zip),
            (zed::Os::Linux, zed::Architecture::Aarch64) => (
                "netcoredbg-linux-arm64.tar.gz",
                zed::DownloadedFileType::GzipTar,
            ),
            (zed::Os::Linux, _) => (
                "netcoredbg-linux-amd64.tar.gz",
                zed::DownloadedFileType::GzipTar,
            ),
            (zed::Os::Mac, zed::Architecture::Aarch64) => {
                ("netcoredbg-osx-arm64.zip", zed::DownloadedFileType::Zip)
            }
            (zed::Os::Mac, _) => (
                "netcoredbg-osx-amd64.tar.gz",
                zed::DownloadedFileType::GzipTar,
            ),
        };

        let version_directory = match zed::latest_github_release(
            REPOSITORY,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        ) {
            Ok(release) => {
                let version_directory = format!("{}-{}", Self::ADAPTER_NAME, release.version);
                if find_binary(&version_directory).is_none() {
                    let asset = release
                        .assets
                        .iter()
                        .find(|asset| asset.name == asset_name)
                        .ok_or_else(|| {
                            format!("netcoredbg {} has no asset '{asset_name}'", release.version)
                        })?;
                    zed::download_file(&asset.download_url, &version_directory, file_type)
                        .map_err(|error| format!("failed to download netcoredbg: {error}"))?;
                    remove_other_versions(Self::ADAPTER_NAME, &version_directory);
                }
                version_directory
            }
            Err(error) => newest_installed_version(Self::ADAPTER_NAME).ok_or(error)?,
        };

        let relative_path = find_binary(&version_directory)
            .ok_or_else(|| format!("netcoredbg binary not found in '{version_directory}'"))?;
        make_executable(&relative_path)?;
        let path = absolute_path(&relative_path)?;
        self.cached_path = Some(path.clone());
        Ok(path)
    }
}

/// Release archives contain a top-level `netcoredbg` directory.
fn find_binary(version_directory: &str) -> Option<String> {
    [
        "netcoredbg/netcoredbg.exe",
        "netcoredbg/netcoredbg",
        "netcoredbg.exe",
        "netcoredbg",
    ]
    .into_iter()
    .map(|file| format!("{version_directory}/{file}"))
    .find(|path| fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
}
