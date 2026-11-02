use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use collections::{FxHashMap, HashMap};
use dap::{DapLocator, DebugRequest, adapters::DebugAdapterName};
use gpui::{BackgroundExecutor, SharedString};
use serde::Deserialize;
use task::{BuildTaskDefinition, DebugScenario, LaunchRequest, SpawnInTerminal, TaskTemplate};

use crate::msbuild::{
    LaunchProfile, MsBuildProject, RunConfiguration, RunKind, is_project_file,
    parse_launch_profile, split_command_line,
};

/// The debug adapter the C# extension registers for .NET (Core) programs.
pub const ADAPTER_NAME: &str = "netcoredbg";
pub const LOCATOR_NAME: &str = "dotnet-locator";

/// Carries the launch profile from `dotnet run` to the locator through the build task, whose
/// arguments `dotnet build` would reject. `NO_LAUNCH_PROFILE` stands for `--no-launch-profile`.
const LAUNCH_PROFILE_VARIABLE: &str = "ZED_DOTNET_LAUNCH_PROFILE";
const NO_LAUNCH_PROFILE: &str = "";

/// Turns `dotnet run` tasks into netcoredbg debug scenarios: the scenario builds the project with
/// `dotnet build`, then asks MSBuild where the program is and applies the `launchSettings.json`
/// profile `dotnet run` would have used.
pub struct DotnetLocator;

#[async_trait]
impl DapLocator for DotnetLocator {
    fn name(&self) -> SharedString {
        SharedString::new_static(LOCATOR_NAME)
    }

    async fn create_scenario(
        &self,
        build_config: &TaskTemplate,
        resolved_label: &str,
        adapter: &DebugAdapterName,
    ) -> Option<DebugScenario> {
        if adapter.as_ref() != ADAPTER_NAME || !is_dotnet(&build_config.command) {
            return None;
        }
        let run_arguments = RunArguments::parse(&build_config.args)?;
        let mut build_task = TaskTemplate {
            label: format!("Build for {resolved_label}"),
            command: build_config.command.clone(),
            args: run_arguments.build_args(),
            cwd: build_config.cwd.clone(),
            env: build_config.env.clone(),
            ..TaskTemplate::default()
        };
        if let Some(launch_profile) = run_arguments.launch_profile {
            build_task
                .env
                .insert(LAUNCH_PROFILE_VARIABLE.to_owned(), launch_profile);
        }
        Some(DebugScenario {
            adapter: adapter.0.clone(),
            label: resolved_label.to_owned().into(),
            build: Some(BuildTaskDefinition::Template {
                task_template: build_task,
                locator_name: Some(self.name()),
            }),
            config: serde_json::Value::Null,
            tcp_connection: None,
        })
    }

    async fn run(
        &self,
        build_config: SpawnInTerminal,
        _executor: BackgroundExecutor,
    ) -> Result<DebugRequest> {
        let cwd = build_config
            .cwd
            .clone()
            .context("the .NET build task has no working directory")?;
        let build_arguments = BuildArguments::parse(&build_config.args);
        let project_path = find_project(&cwd, build_arguments.project.as_deref())?;
        let launch_profile = build_config.env.get(LAUNCH_PROFILE_VARIABLE).cloned();
        let launch_profile = match launch_profile.as_deref() {
            Some(NO_LAUNCH_PROFILE) => None,
            name => load_launch_profile(&project_path, name)?,
        };
        let output = query_output(&project_path, &build_arguments).await?;
        Ok(DebugRequest::Launch(launch_request(output, launch_profile)))
    }
}

/// The debug scenario that debugs `configuration` the way `dotnet: run` runs it.
pub fn debug_scenario(
    configuration: &RunConfiguration,
    project: &MsBuildProject,
) -> Result<DebugScenario> {
    let RunKind::DotnetRun {
        launch_profile,
        framework,
    } = &configuration.kind
    else {
        bail!(
            "{} targets the .NET Framework, which netcoredbg can't debug",
            configuration.label()
        );
    };

    let mut args = vec![
        "build".to_owned(),
        project.path.to_string_lossy().into_owned(),
    ];
    if let Some(framework) = framework {
        args.extend(["--framework".to_owned(), framework.clone()]);
    }
    let mut env = HashMap::default();
    env.insert(
        LAUNCH_PROFILE_VARIABLE.to_owned(),
        launch_profile
            .clone()
            .unwrap_or_else(|| NO_LAUNCH_PROFILE.to_owned()),
    );
    Ok(DebugScenario {
        adapter: ADAPTER_NAME.into(),
        label: format!("Debug {}", configuration.label()).into(),
        build: Some(BuildTaskDefinition::Template {
            task_template: TaskTemplate {
                label: format!("Build {}", project.name),
                command: "dotnet".to_owned(),
                args,
                cwd: Some(project.directory().to_string_lossy().into_owned()),
                env,
                ..TaskTemplate::default()
            },
            locator_name: Some(LOCATOR_NAME.into()),
        }),
        config: serde_json::Value::Null,
        tcp_connection: None,
    })
}

fn is_dotnet(command: &str) -> bool {
    Path::new(command)
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case("dotnet"))
}

/// The arguments of a `dotnet run` command that matter for debugging it.
#[derive(Debug, Default, PartialEq)]
struct RunArguments {
    project: Option<String>,
    framework: Option<String>,
    configuration: Option<String>,
    /// `Some("")` for `--no-launch-profile`.
    launch_profile: Option<String>,
}

impl RunArguments {
    fn parse(args: &[String]) -> Option<Self> {
        let mut args = args.iter();
        if args.next()? != "run" {
            return None;
        }
        let mut run_arguments = Self::default();
        while let Some(arg) = args.next() {
            let (option, inline_value) = match arg.split_once('=') {
                Some((option, value)) if option.starts_with('-') => {
                    (option, Some(value.to_owned()))
                }
                _ => (arg.as_str(), None),
            };
            let mut value = || inline_value.clone().or_else(|| args.next().cloned());
            match option {
                // Program arguments follow; launch profiles supply them when debugging.
                "--" => break,
                "--project" => run_arguments.project = value(),
                "-f" | "--framework" => run_arguments.framework = value(),
                "-c" | "--configuration" => run_arguments.configuration = value(),
                "-lp" | "--launch-profile" => run_arguments.launch_profile = value(),
                "--no-launch-profile" => {
                    run_arguments.launch_profile = Some(NO_LAUNCH_PROFILE.to_owned())
                }
                _ => {}
            }
        }
        Some(run_arguments)
    }

    fn build_args(&self) -> Vec<String> {
        let mut args = vec!["build".to_owned()];
        args.extend(self.project.clone());
        if let Some(framework) = &self.framework {
            args.extend(["--framework".to_owned(), framework.clone()]);
        }
        if let Some(configuration) = &self.configuration {
            args.extend(["--configuration".to_owned(), configuration.clone()]);
        }
        args
    }
}

/// The arguments of the `dotnet build` command a scenario built.
#[derive(Debug, Default, PartialEq)]
struct BuildArguments {
    project: Option<String>,
    framework: Option<String>,
    configuration: Option<String>,
}

impl BuildArguments {
    fn parse(args: &[String]) -> Self {
        let mut build_arguments = Self::default();
        let mut args = args.iter().skip_while(|arg| *arg != "build").skip(1);
        while let Some(arg) = args.next() {
            let (option, inline_value) = match arg.split_once('=') {
                Some((option, value)) if option.starts_with('-') => {
                    (option, Some(value.to_owned()))
                }
                _ => (arg.as_str(), None),
            };
            let mut value = || inline_value.clone().or_else(|| args.next().cloned());
            match option {
                "-f" | "--framework" => build_arguments.framework = value(),
                "-c" | "--configuration" => build_arguments.configuration = value(),
                option if option.starts_with('-') => {}
                _ => build_arguments.project = Some(arg.clone()),
            }
        }
        build_arguments
    }
}

/// Resolves the project `dotnet build` built: `project` (a file or a directory), or the project
/// in `cwd`.
fn find_project(cwd: &Path, project: Option<&str>) -> Result<PathBuf> {
    let path = match project {
        Some(project) => cwd.join(project),
        None => cwd.to_path_buf(),
    };
    if path.is_file() {
        return Ok(path);
    }
    let projects = std::fs::read_dir(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.is_file() && is_project_file(path))
        .collect::<Vec<_>>();
    match projects.as_slice() {
        [project] => Ok(project.clone()),
        [] => Err(anyhow!("no .NET project found in {}", path.display())),
        _ => Err(anyhow!(
            "{} contains several projects; pass one with --project",
            path.display()
        )),
    }
}

fn load_launch_profile(project_path: &Path, name: Option<&str>) -> Result<Option<LaunchProfile>> {
    let project_directory = project_path.parent().unwrap_or(Path::new(""));
    // Visual Basic projects keep their launch settings under "My Project".
    for directory in ["Properties", "My Project"] {
        let path = project_directory
            .join(directory)
            .join("launchSettings.json");
        if path.is_file() {
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let profile = parse_launch_profile(&contents, name)
                .with_context(|| format!("parsing {}", path.display()))?;
            if let Some(name) = name
                && profile.is_none()
            {
                bail!("{} has no launch profile named {name}", path.display());
            }
            return Ok(profile);
        }
    }
    match name {
        Some(name) => {
            bail!("launch profile {name} not found: the project has no launchSettings.json")
        }
        None => Ok(None),
    }
}

/// Where the build put the program, as MSBuild reports it.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct ProjectOutput {
    #[serde(default)]
    target_path: String,
    #[serde(default)]
    run_working_directory: String,
    #[serde(default)]
    target_frameworks: String,
    #[serde(skip)]
    project_directory: PathBuf,
}

async fn query_output(project_path: &Path, build: &BuildArguments) -> Result<ProjectOutput> {
    let configuration = build.configuration.as_deref();
    let mut output =
        msbuild_properties(project_path, build.framework.as_deref(), configuration).await?;
    // Multi-targeting projects only have an output per target framework; `dotnet run` asks for
    // one too, unless there's only one that can run.
    if output.target_path.is_empty()
        && build.framework.is_none()
        && let Some(first_framework) = output
            .target_frameworks
            .split(';')
            .map(str::trim)
            .find(|framework| !framework.is_empty())
    {
        output = msbuild_properties(project_path, Some(first_framework), configuration).await?;
    }
    if output.target_path.is_empty() {
        bail!(
            "MSBuild did not report an output assembly for {}",
            project_path.display()
        );
    }
    output.project_directory = project_path.parent().unwrap_or(Path::new("")).to_path_buf();
    Ok(output)
}

async fn msbuild_properties(
    project_path: &Path,
    framework: Option<&str>,
    configuration: Option<&str>,
) -> Result<ProjectOutput> {
    #[derive(Deserialize)]
    struct Output {
        #[serde(rename = "Properties")]
        properties: ProjectOutput,
    }

    let mut command = util::command::new_command("dotnet");
    command
        .arg("msbuild")
        .arg(project_path)
        .args([
            "-nologo",
            "-getProperty:TargetPath",
            "-getProperty:RunWorkingDirectory",
            "-getProperty:TargetFrameworks",
        ])
        .current_dir(project_path.parent().unwrap_or(Path::new(".")));
    command.arg(format!(
        "-p:Configuration={}",
        configuration.unwrap_or("Debug")
    ));
    if let Some(framework) = framework {
        command.arg(format!("-p:TargetFramework={framework}"));
    }
    let output = command.output().await.context("running dotnet msbuild")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        bail!(
            "dotnet msbuild failed for {}: {}{}",
            project_path.display(),
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let json_start = stdout
        .find('{')
        .context("dotnet msbuild printed no properties")?;
    let output: Output = serde_json::from_str(&stdout[json_start..])
        .with_context(|| format!("parsing dotnet msbuild output: {stdout}"))?;
    Ok(output.properties)
}

fn launch_request(output: ProjectOutput, launch_profile: Option<LaunchProfile>) -> LaunchRequest {
    let mut env = FxHashMap::default();
    let mut args = Vec::new();
    // `dotnet run` starts programs in the project directory unless the project says otherwise.
    let mut cwd = if output.run_working_directory.is_empty() {
        output.project_directory.clone()
    } else {
        PathBuf::from(&output.run_working_directory)
    };
    if let Some(profile) = launch_profile {
        if let Some(application_url) = profile.application_url {
            env.insert("ASPNETCORE_URLS".to_owned(), application_url);
        }
        env.extend(profile.environment_variables);
        if let Some(command_line_args) = profile.command_line_args {
            args = split_command_line(&command_line_args);
        }
        if let Some(working_directory) = profile.working_directory {
            cwd = output.project_directory.join(working_directory);
        }
    }
    LaunchRequest {
        program: output.target_path,
        cwd: Some(cwd),
        args,
        env,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn parses_run_arguments() {
        assert_eq!(RunArguments::parse(&strings(&["build"])), None);
        assert_eq!(
            RunArguments::parse(&strings(&[
                "run",
                "--project",
                "src/Api/Api.csproj",
                "-lp",
                "https",
                "--framework=net8.0",
                "--",
                "--project",
                "ignored",
            ])),
            Some(RunArguments {
                project: Some("src/Api/Api.csproj".into()),
                framework: Some("net8.0".into()),
                configuration: None,
                launch_profile: Some("https".into()),
            })
        );
        let run_arguments = RunArguments::parse(&strings(&["run", "--no-launch-profile"]));
        assert_eq!(
            run_arguments
                .as_ref()
                .and_then(|arguments| arguments.launch_profile.as_deref()),
            Some(NO_LAUNCH_PROFILE)
        );
        assert_eq!(
            run_arguments.map(|arguments| arguments.build_args()),
            Some(strings(&["build"]))
        );
    }

    #[test]
    fn parses_build_arguments() {
        assert_eq!(
            BuildArguments::parse(&strings(&[
                "build",
                "C:\\work\\Api\\Api.csproj",
                "--framework",
                "net8.0",
                "-c",
                "Debug",
            ])),
            BuildArguments {
                project: Some("C:\\work\\Api\\Api.csproj".into()),
                framework: Some("net8.0".into()),
                configuration: Some("Debug".into()),
            }
        );
    }

    #[test]
    fn applies_launch_profiles() {
        let project_directory = PathBuf::from("work").join("Api");
        let output = ProjectOutput {
            target_path: "Api.dll".into(),
            project_directory: project_directory.clone(),
            ..ProjectOutput::default()
        };
        let request = launch_request(
            output,
            Some(LaunchProfile {
                name: "https".into(),
                command_line_args: Some("--seed \"two words\"".into()),
                working_directory: Some("content".into()),
                application_url: Some("https://localhost:7265;http://localhost:5109".into()),
                environment_variables: vec![(
                    "ASPNETCORE_ENVIRONMENT".into(),
                    "Development".into(),
                )],
            }),
        );
        assert_eq!(request.program, "Api.dll");
        assert_eq!(request.cwd, Some(project_directory.join("content")));
        assert_eq!(request.args, strings(&["--seed", "two words"]));
        assert_eq!(
            request.env.get("ASPNETCORE_URLS").map(String::as_str),
            Some("https://localhost:7265;http://localhost:5109")
        );
        assert_eq!(
            request
                .env
                .get("ASPNETCORE_ENVIRONMENT")
                .map(String::as_str),
            Some("Development")
        );
    }
}
