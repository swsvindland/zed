// Download and settings handling adapted from https://github.com/zed-extensions/csharp (Apache-2.0).

use std::fs;

use zed_extension_api::{self as zed, LanguageServerId, Result, serde_json, settings::LspSettings};

use crate::nuget::NuGetClient;

const PACKAGE_PREFIX: &str = "roslyn-language-server";
const SERVER_BINARY: &str = "Microsoft.CodeAnalysis.LanguageServer";

#[derive(Default)]
pub struct Roslyn {
    cached_command: Option<ServerCommand>,
    nuget: NuGetClient,
}

#[derive(Clone)]
enum ServerCommand {
    Executable(String),
    /// Framework-dependent build, run through `dotnet exec`.
    Assembly(String),
}

impl ServerCommand {
    fn path(&self) -> &str {
        match self {
            ServerCommand::Executable(path) | ServerCommand::Assembly(path) => path,
        }
    }

    fn into_command(
        self,
        user_arguments: Option<Vec<String>>,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let mut arguments = user_arguments.unwrap_or_else(|| {
            vec![
                "--stdio".to_string(),
                "--autoLoadProjects".to_string(),
                "--logLevel=Information".to_string(),
            ]
        });
        match self {
            ServerCommand::Executable(path) => Ok(zed::Command {
                command: path,
                args: arguments,
                env: Default::default(),
            }),
            ServerCommand::Assembly(path) => {
                // Zed resolves relative commands against the extension directory, not `PATH`.
                let dotnet = worktree
                    .which("dotnet")
                    .ok_or("the .NET SDK (`dotnet`) was not found on PATH")?;
                arguments.splice(0..0, ["exec".to_string(), path]);
                Ok(zed::Command {
                    command: dotnet,
                    args: arguments,
                    env: Default::default(),
                })
            }
        }
    }
}

impl Roslyn {
    pub const LANGUAGE_SERVER_ID: &'static str = "roslyn";

    pub fn command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let binary_settings = LspSettings::for_worktree(Self::LANGUAGE_SERVER_ID, worktree)
            .ok()
            .and_then(|settings| settings.binary);
        let user_arguments = binary_settings
            .as_ref()
            .and_then(|binary| binary.arguments.clone());

        if let Some(path) = binary_settings.and_then(|binary| binary.path) {
            return ServerCommand::Executable(path).into_command(user_arguments, worktree);
        }

        if let Some(cached) = &self.cached_command
            && fs::metadata(cached.path()).is_ok_and(|metadata| metadata.is_file())
        {
            return cached.clone().into_command(user_arguments, worktree);
        }

        let command = self.install(language_server_id)?;
        self.cached_command = Some(command.clone());
        command.into_command(user_arguments, worktree)
    }

    fn install(&mut self, language_server_id: &LanguageServerId) -> Result<ServerCommand> {
        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );

        let runtime_identifier = runtime_identifier()?;
        let package_id = format!("{PACKAGE_PREFIX}.{runtime_identifier}");
        let version_directory = match self.nuget.latest_version(&package_id) {
            Ok(version) => {
                let version_directory = format!("{}-{version}", Self::LANGUAGE_SERVER_ID);
                if find_server(runtime_identifier, &version_directory).is_err() {
                    zed::set_language_server_installation_status(
                        language_server_id,
                        &zed::LanguageServerInstallationStatus::Downloading,
                    );
                    self.nuget
                        .download_and_extract(&package_id, &version, &version_directory)?;
                    remove_other_versions(Self::LANGUAGE_SERVER_ID, &version_directory);
                }
                version_directory
            }
            // Work offline with whatever version was downloaded last.
            Err(error) => newest_installed_version(Self::LANGUAGE_SERVER_ID).ok_or(error)?,
        };

        find_server(runtime_identifier, &version_directory)
    }

    pub fn workspace_configuration(worktree: &zed::Worktree) -> serde_json::Value {
        // Roslyn asks for each setting with a `workspace/configuration` request whose section is
        // the full `language|category.option` key, and Zed answers those with a top-level key
        // lookup, so the configuration has to be flat.
        let mut configuration = serde_json::json!({
            // Roslyn's code lens commands are client-side VS Code commands that Zed can't run.
            "csharp|code_lens.dotnet_enable_references_code_lens": false,
            "csharp|code_lens.dotnet_enable_tests_code_lens": false,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_parameters": true,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_literal_parameters": true,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_indexer_parameters": true,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_object_creation_parameters": true,
            "csharp|inlay_hints.dotnet_enable_inlay_hints_for_other_parameters": true,
            "csharp|inlay_hints.csharp_enable_inlay_hints_for_types": true,
            "csharp|inlay_hints.csharp_enable_inlay_hints_for_implicit_variable_types": true,
            "csharp|inlay_hints.csharp_enable_inlay_hints_for_lambda_parameter_types": true,
            "csharp|inlay_hints.csharp_enable_inlay_hints_for_implicit_object_creation": true,
            "csharp|background_analysis.dotnet_analyzer_diagnostics_scope": "openFiles",
            "csharp|background_analysis.dotnet_compiler_diagnostics_scope": "fullSolution",
            "csharp|completion.dotnet_show_completion_items_from_unimported_namespaces": true,
            "csharp|completion.dotnet_provide_regex_completions": true,
        });

        let user_settings = LspSettings::for_worktree(Self::LANGUAGE_SERVER_ID, worktree)
            .ok()
            .and_then(|settings| settings.settings);
        if let (Some(configuration), Some(serde_json::Value::Object(user_settings))) =
            (configuration.as_object_mut(), user_settings)
        {
            for (key, value) in user_settings {
                match value {
                    // `{ "csharp|inlay_hints": { "option": true } }`
                    serde_json::Value::Object(nested) if key.contains('|') => {
                        for (nested_key, nested_value) in nested {
                            configuration.insert(format!("{key}.{nested_key}"), nested_value);
                        }
                    }
                    // `{ "csharp|inlay_hints.option": true }`
                    value => {
                        configuration.insert(key, value);
                    }
                }
            }
        }
        configuration
    }
}

fn runtime_identifier() -> Result<&'static str> {
    Ok(match zed::current_platform() {
        (zed::Os::Windows, zed::Architecture::X8664) => "win-x64",
        (zed::Os::Windows, zed::Architecture::Aarch64) => "win-arm64",
        (zed::Os::Linux, zed::Architecture::X8664) => "linux-x64",
        (zed::Os::Linux, zed::Architecture::Aarch64) => "linux-arm64",
        (zed::Os::Mac, zed::Architecture::X8664) => "osx-x64",
        (zed::Os::Mac, zed::Architecture::Aarch64) => "osx-arm64",
        (os, architecture) => {
            return Err(format!(
                "Roslyn has no build for {os:?} on {architecture:?}"
            ));
        }
    })
}

/// The package layout is `tools/<target framework>/<runtime identifier>/`.
fn find_server(runtime_identifier: &str, version_directory: &str) -> Result<ServerCommand> {
    let tools_directory = format!("{version_directory}/tools");
    let entries = fs::read_dir(&tools_directory)
        .map_err(|error| format!("failed to read '{tools_directory}': {error}"))?;
    for entry in entries.flatten() {
        let server_directory = entry.path().join(runtime_identifier);
        let candidates = [
            (format!("{SERVER_BINARY}.exe"), true),
            (SERVER_BINARY.to_string(), true),
            (format!("{SERVER_BINARY}.dll"), false),
        ];
        for (file_name, is_executable) in candidates {
            let path = server_directory.join(&file_name);
            if fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                let relative_path = path.to_string_lossy();
                let absolute = absolute_path(&relative_path)?;
                // Windows can't start programs whose path exceeds MAX_PATH, but `dotnet` can
                // load an assembly from such a path, and every build includes the assembly.
                let exceeds_max_path =
                    absolute.len() >= 260 && zed::current_platform().0 == zed::Os::Windows;
                if !is_executable || exceeds_max_path {
                    let assembly = absolute_path(
                        &server_directory
                            .join(format!("{SERVER_BINARY}.dll"))
                            .to_string_lossy(),
                    )?;
                    return Ok(ServerCommand::Assembly(assembly));
                }
                make_executable(&relative_path)?;
                return Ok(ServerCommand::Executable(absolute));
            }
        }
    }
    Err(format!(
        "no {SERVER_BINARY} found under '{tools_directory}'"
    ))
}

/// Marks a downloaded binary executable. `path` is relative to the extension's directory. Windows
/// has no executable bit, so this is skipped there.
pub(crate) fn make_executable(path: &str) -> Result<()> {
    if zed::current_platform().0 == zed::Os::Windows {
        return Ok(());
    }
    zed::make_file_executable(path)
}

pub(crate) fn absolute_path(relative_path: &str) -> Result<String> {
    let working_directory = std::env::current_dir()
        .map_err(|error| format!("failed to resolve the extension directory: {error}"))?;
    Ok(working_directory
        .join(relative_path)
        .to_string_lossy()
        .into_owned())
}

pub(crate) fn remove_other_versions(prefix: &str, keep: &str) {
    let Ok(entries) = fs::read_dir(".") else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with(&format!("{prefix}-")) && file_name != keep {
            fs::remove_dir_all(entry.path()).ok();
        }
    }
}

pub(crate) fn newest_installed_version(prefix: &str) -> Option<String> {
    fs::read_dir(".")
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|file_name| file_name.starts_with(&format!("{prefix}-")))
        .max()
}
