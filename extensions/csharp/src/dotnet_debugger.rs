//! [dotnet-debugger](https://github.com/dvislobokov/dotnet-debugger) (MIT): an ICorDebug-based
//! adapter that, unlike netcoredbg, also debugs .NET Framework 4.x programs on Windows. It only
//! debugs 64-bit processes.

use std::fs;

use zed_extension_api::{self as zed, Result};

use crate::{
    dap_config,
    nuget::NuGetClient,
    roslyn::{absolute_path, newest_installed_version, remove_other_versions},
};

/// A `dotnet tool` package: the adapter is a framework-dependent assembly that runs on .NET 8 or
/// later.
const PACKAGE_ID: &str = "dotnet-debugger-dap";
const ASSEMBLY: &str = "dotnet-debugger.dll";

#[derive(Default)]
pub struct DotnetDebugger {
    cached_path: Option<String>,
    nuget: NuGetClient,
}

impl DotnetDebugger {
    pub const ADAPTER_NAME: &'static str = "dotnet-debugger";

    pub fn binary(
        &mut self,
        config: zed::DebugTaskDefinition,
        user_provided_path: Option<String>,
        worktree: &zed::Worktree,
    ) -> Result<zed::DebugAdapterBinary> {
        let request = dap_config::parse(Self::ADAPTER_NAME, &config)?;

        // `dotnet tool install -g dotnet-debugger-dap` puts a `dotnet-debugger` command on PATH.
        let (command, arguments) = match user_provided_path
            .or_else(|| worktree.which(Self::ADAPTER_NAME))
        {
            Some(path) if path.to_lowercase().ends_with(".dll") => (dotnet(worktree)?, vec![path]),
            Some(path) => (path, Vec::new()),
            None => {
                let assembly = self.install()?;
                (dotnet(worktree)?, vec![assembly])
            }
        };

        Ok(zed::DebugAdapterBinary {
            command: Some(command),
            arguments,
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

        let version_directory = match self.nuget.latest_version(PACKAGE_ID) {
            Ok(version) => {
                let version_directory = format!("{}-{version}", Self::ADAPTER_NAME);
                if find_assembly(&version_directory).is_none() {
                    self.nuget
                        .download_and_extract(PACKAGE_ID, &version, &version_directory)?;
                    remove_other_versions(Self::ADAPTER_NAME, &version_directory);
                }
                version_directory
            }
            // Work offline with whatever version was downloaded last.
            Err(error) => newest_installed_version(Self::ADAPTER_NAME).ok_or(error)?,
        };

        let relative_path = find_assembly(&version_directory).ok_or_else(|| {
            format!("{ASSEMBLY} not found in '{version_directory}'; delete the directory to download it again")
        })?;
        let path = absolute_path(&relative_path)?;
        self.cached_path = Some(path.clone());
        Ok(path)
    }
}

fn dotnet(worktree: &zed::Worktree) -> Result<String> {
    // Zed resolves relative commands against the extension directory, not `PATH`.
    worktree.which("dotnet").ok_or_else(|| {
        "dotnet-debugger runs on .NET 8 or later, but `dotnet` was not found on PATH".to_string()
    })
}

/// The package keeps the assembly in `tools/<tfm>/any/`.
fn find_assembly(version_directory: &str) -> Option<String> {
    let tools = format!("{version_directory}/tools");
    fs::read_dir(&tools)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .map(|framework| format!("{tools}/{framework}/any/{ASSEMBLY}"))
        .find(|path| fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
}
