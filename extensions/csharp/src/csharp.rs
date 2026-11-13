mod dap_config;
mod dotnet_debugger;
mod netcoredbg;
mod nuget;
mod roslyn;

use zed_extension_api::{self as zed, Result, serde_json};

use crate::{dotnet_debugger::DotnetDebugger, netcoredbg::Netcoredbg, roslyn::Roslyn};

#[derive(Default)]
struct CsharpExtension {
    roslyn: Roslyn,
    netcoredbg: Netcoredbg,
    dotnet_debugger: DotnetDebugger,
}

fn is_known_adapter(adapter_name: &str) -> Result<()> {
    match adapter_name {
        Netcoredbg::ADAPTER_NAME | DotnetDebugger::ADAPTER_NAME => Ok(()),
        other => Err(format!("unknown debug adapter: {other}")),
    }
}

impl zed::Extension for CsharpExtension {
    fn new() -> Self {
        Self::default()
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        match language_server_id.as_ref() {
            Roslyn::LANGUAGE_SERVER_ID => self.roslyn.command(language_server_id, worktree),
            other => Err(format!("unknown language server: {other}")),
        }
    }

    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        match language_server_id.as_ref() {
            Roslyn::LANGUAGE_SERVER_ID => Ok(Some(Roslyn::workspace_configuration(worktree))),
            _ => Ok(None),
        }
    }

    fn get_dap_binary(
        &mut self,
        adapter_name: String,
        config: zed::DebugTaskDefinition,
        user_provided_debug_adapter_path: Option<String>,
        worktree: &zed::Worktree,
    ) -> Result<zed::DebugAdapterBinary, String> {
        match adapter_name.as_str() {
            Netcoredbg::ADAPTER_NAME => {
                self.netcoredbg
                    .binary(config, user_provided_debug_adapter_path, worktree)
            }
            DotnetDebugger::ADAPTER_NAME => {
                self.dotnet_debugger
                    .binary(config, user_provided_debug_adapter_path, worktree)
            }
            other => Err(format!("unknown debug adapter: {other}")),
        }
    }

    fn dap_request_kind(
        &mut self,
        adapter_name: String,
        config: serde_json::Value,
    ) -> Result<zed::StartDebuggingRequestArgumentsRequest, String> {
        is_known_adapter(&adapter_name)?;
        dap_config::request_kind(&config)
    }

    fn dap_config_to_scenario(
        &mut self,
        config: zed::DebugConfig,
    ) -> Result<zed::DebugScenario, String> {
        is_known_adapter(&config.adapter)?;
        dap_config::config_to_scenario(config)
    }
}

zed::register_extension!(CsharpExtension);
