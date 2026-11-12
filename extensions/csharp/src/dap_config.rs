//! Debug configuration handling shared by the .NET debug adapters, which take the same `launch`
//! and `attach` arguments.

use zed_extension_api::{self as zed, Result, serde_json};

pub fn request_kind(
    configuration: &serde_json::Value,
) -> Result<zed::StartDebuggingRequestArgumentsRequest> {
    match configuration
        .get("request")
        .and_then(|request| request.as_str())
    {
        Some("launch") => Ok(zed::StartDebuggingRequestArgumentsRequest::Launch),
        Some("attach") => Ok(zed::StartDebuggingRequestArgumentsRequest::Attach),
        Some(other) => Err(format!(
            "invalid request '{other}', expected 'launch' or 'attach'"
        )),
        None => Err("missing 'request' (expected 'launch' or 'attach')".to_string()),
    }
}

/// Turns Zed's adapter-agnostic configuration (from `dotnet` run tasks and the attach picker)
/// into adapter arguments.
pub fn config_to_scenario(config: zed::DebugConfig) -> Result<zed::DebugScenario> {
    let configuration = match config.request {
        zed::DebugRequest::Launch(launch) => {
            let environment = launch
                .envs
                .into_iter()
                .map(|(key, value)| (key, serde_json::Value::String(value)))
                .collect::<serde_json::Map<_, _>>();
            let mut configuration = serde_json::json!({
                "request": "launch",
                "program": launch.program,
                "args": launch.args,
                "env": environment,
                "stopAtEntry": config.stop_on_entry.unwrap_or(false),
            });
            if let Some(cwd) = launch.cwd {
                configuration["cwd"] = serde_json::Value::String(cwd);
            }
            configuration
        }
        zed::DebugRequest::Attach(attach) => {
            let mut configuration = serde_json::json!({ "request": "attach" });
            if let Some(process_id) = attach.process_id {
                configuration["processId"] = serde_json::Value::from(process_id);
            }
            configuration
        }
    };

    Ok(zed::DebugScenario {
        label: config.label,
        adapter: config.adapter,
        build: None,
        config: configuration.to_string(),
        tcp_connection: None,
    })
}

/// Parses a debug task's configuration and tells whether it launches or attaches.
pub fn parse(
    adapter_name: &str,
    config: &zed::DebugTaskDefinition,
) -> Result<zed::StartDebuggingRequestArgumentsRequest> {
    let configuration: serde_json::Value = serde_json::from_str(&config.config)
        .map_err(|error| format!("invalid {adapter_name} configuration: {error}"))?;
    request_kind(&configuration)
}
