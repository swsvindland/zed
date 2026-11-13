use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use collections::{FxHashMap, HashMap};
use dap::{DapLocator, DebugRequest, adapters::DebugAdapterName};
use gpui::{App, BackgroundExecutor, Global, SharedString};
use language::{LanguageName, language_settings::AllLanguageSettings};
use serde::Deserialize;
use settings::{Settings as _, SettingsStore};
use task::{BuildTaskDefinition, DebugScenario, LaunchRequest, SpawnInTerminal, TaskTemplate};

use crate::msbuild::{
    CommandLine, LaunchProfile, MsBuildProject, RunConfiguration, RunKind,
    executable_output_directory, executable_path, is_net_framework, is_project_file,
    parse_launch_profile, parse_project, split_command_line,
};

/// The debug adapter the C# extension registers for .NET (Core) programs.
pub const NETCOREDBG: &str = "netcoredbg";
/// The debug adapter the C# extension registers for .NET Framework programs, which netcoredbg
/// can't debug. It debugs .NET 8 and later as well, but only 64-bit processes.
pub const DOTNET_DEBUGGER: &str = "dotnet-debugger";
pub const LOCATOR_NAME: &str = "dotnet-locator";

/// Carries the launch profile from `dotnet run` to the locator through the build task, whose
/// arguments `dotnet build` would reject. `NO_LAUNCH_PROFILE` stands for `--no-launch-profile`.
const LAUNCH_PROFILE_VARIABLE: &str = "ZED_DOTNET_LAUNCH_PROFILE";
const NO_LAUNCH_PROFILE: &str = "";
/// Tells the locator which adapter the scenario was created for, which is decided before the
/// build tells which framework the program targets.
const ADAPTER_VARIABLE: &str = "ZED_DOTNET_DEBUG_ADAPTER";

/// The adapter for a program that targets `framework`: dotnet-debugger for the .NET Framework,
/// and otherwise `preferred` (netcoredbg, unless the user prefers dotnet-debugger).
pub fn adapter_for(framework: Option<&str>, preferred: &str) -> &'static str {
    if preferred == DOTNET_DEBUGGER || framework.is_some_and(is_net_framework) {
        DOTNET_DEBUGGER
    } else {
        NETCOREDBG
    }
}

fn is_dotnet_adapter(adapter: &str) -> bool {
    adapter == NETCOREDBG || adapter == DOTNET_DEBUGGER
}

/// Turns `dotnet run` tasks into debug scenarios: the scenario builds the project with
/// `dotnet build`, then asks MSBuild where the program is and applies the `launchSettings.json`
/// profile `dotnet run` would have used.
#[derive(Default)]
pub struct DotnetLocator {
    /// Whether the user's settings make dotnet-debugger the C# debugger. The adapter that callers
    /// pass can't tell: when the settings name no debugger, they pass the C# language's first one,
    /// which is dotnet-debugger too, so that the attach picker starts on it.
    user_prefers_dotnet_debugger: AtomicBool,
}

struct GlobalDotnetLocator(Arc<DotnetLocator>);

impl Global for GlobalDotnetLocator {}

impl DotnetLocator {
    /// The locator, which follows the user's C# debugger setting.
    pub fn global(cx: &mut App) -> Arc<Self> {
        if let Some(locator) = cx.try_global::<GlobalDotnetLocator>() {
            return locator.0.clone();
        }
        let locator = Arc::new(Self::default());
        locator.read_settings(cx);
        cx.observe_global::<SettingsStore>({
            let locator = locator.clone();
            move |cx| locator.read_settings(cx)
        })
        .detach();
        cx.set_global(GlobalDotnetLocator(locator.clone()));
        locator
    }

    fn read_settings(&self, cx: &App) {
        let settings = AllLanguageSettings::get_global(cx).language(
            None,
            Some(&LanguageName::new_static("C#")),
            cx,
        );
        let prefers_dotnet_debugger = settings
            .debuggers
            .first()
            .is_some_and(|debugger| debugger == DOTNET_DEBUGGER);
        self.user_prefers_dotnet_debugger
            .store(prefers_dotnet_debugger, Ordering::Relaxed);
    }
}

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
        if !is_dotnet_adapter(adapter.as_ref()) || !is_dotnet(&build_config.command) {
            return None;
        }
        let run_arguments = RunArguments::parse(&build_config.args)?;
        let framework = run_arguments.framework.clone().or_else(|| {
            let cwd = build_config.cwd.as_deref()?;
            target_framework(Path::new(cwd), run_arguments.project.as_deref())
        });
        let preferred = if self.user_prefers_dotnet_debugger.load(Ordering::Relaxed) {
            adapter.as_ref()
        } else {
            NETCOREDBG
        };
        let adapter = adapter_for(framework.as_deref(), preferred);

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
        build_task
            .env
            .insert(ADAPTER_VARIABLE.to_owned(), adapter.to_owned());
        Some(DebugScenario {
            adapter: adapter.into(),
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
        let adapter = build_config
            .env
            .get(ADAPTER_VARIABLE)
            .map_or(NETCOREDBG, String::as_str);
        check_adapter(adapter, &project_path, &output)?;
        Ok(DebugRequest::Launch(launch_request(output, launch_profile)))
    }
}

/// The framework a `dotnet run` task in `cwd` would build, when the project can be found without
/// the task's variables (`$ZED_DIRNAME`) resolved.
fn target_framework(cwd: &Path, project: Option<&str>) -> Option<String> {
    if cwd.to_string_lossy().contains('$') || project.is_some_and(|project| project.contains('$')) {
        return None;
    }
    let project_path = find_project(cwd, project).ok()?;
    let contents = std::fs::read_to_string(&project_path).ok()?;
    let project = parse_project(&project_path, &contents).ok()?;
    // Like `query_output`, a multi-targeting project is debugged in its first framework.
    project.target_frameworks.into_iter().next()
}

/// Fails with an explanation when netcoredbg was picked for a .NET Framework program, which
/// happens when the locator couldn't read the project before the build.
fn check_adapter(adapter: &str, project_path: &Path, output: &ProjectOutput) -> Result<()> {
    if adapter == NETCOREDBG && is_net_framework(&output.target_framework) {
        let name = project_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        bail!(
            "{name} targets {}, which netcoredbg can't debug. Start it with `dotnet: debug`, or \
             make dotnet-debugger the C# debugger with \"languages\": {{ \"C#\": {{ \
             \"debuggers\": [\"dotnet-debugger\"] }} }}",
            output.target_framework
        );
    }
    Ok(())
}

/// The debug scenario that debugs a `dotnet run` configuration the way `dotnet: run` runs it.
pub fn dotnet_run_scenario(
    configuration: &RunConfiguration,
    project: &MsBuildProject,
) -> Result<DebugScenario> {
    let RunKind::DotnetRun {
        launch_profile,
        framework,
    } = &configuration.kind
    else {
        bail!("{} is not started with dotnet run", configuration.label());
    };
    let target_framework = framework
        .as_deref()
        .or_else(|| project.target_frameworks.first().map(String::as_str));
    let adapter = adapter_for(target_framework, NETCOREDBG);

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
    env.insert(ADAPTER_VARIABLE.to_owned(), adapter.to_owned());
    Ok(DebugScenario {
        adapter: adapter.into(),
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

/// The debug scenario for a legacy executable: `build` (MSBuild.exe) builds it, then
/// dotnet-debugger starts it from its output directory, as Visual Studio does.
pub fn executable_scenario(
    configuration: &RunConfiguration,
    project: &MsBuildProject,
    build: CommandLine,
) -> DebugScenario {
    DebugScenario {
        adapter: DOTNET_DEBUGGER.into(),
        label: format!("Debug {}", configuration.label()).into(),
        build: Some(BuildTaskDefinition::Template {
            task_template: TaskTemplate {
                label: format!("Build {}", project.name),
                command: build.program,
                args: build.args,
                cwd: Some(project.directory().to_string_lossy().into_owned()),
                ..TaskTemplate::default()
            },
            locator_name: None,
        }),
        config: serde_json::json!({
            "request": "launch",
            "program": executable_path(project),
            "cwd": executable_output_directory(project),
            "args": [],
            "env": {},
        }),
        tcp_connection: None,
    }
}

/// The debug scenario that attaches dotnet-debugger to a running process.
pub fn attach_scenario(label: String, process_id: u32) -> DebugScenario {
    DebugScenario {
        adapter: DOTNET_DEBUGGER.into(),
        label: label.into(),
        build: None,
        config: serde_json::json!({ "request": "attach", "processId": process_id }),
        tcp_connection: None,
    }
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
    target_framework: String,
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
            "-getProperty:TargetFramework",
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

    fn adapter_of(scenario: &DebugScenario) -> &str {
        scenario.adapter.as_ref()
    }

    fn build_env(scenario: &DebugScenario) -> &HashMap<String, String> {
        match &scenario.build {
            Some(BuildTaskDefinition::Template { task_template, .. }) => &task_template.env,
            other => panic!("unexpected build: {other:?}"),
        }
    }

    fn dotnet_run_task(args: &[&str], cwd: Option<&Path>) -> TaskTemplate {
        TaskTemplate {
            label: "dotnet run".into(),
            command: "dotnet".into(),
            args: strings(args),
            cwd: cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
            ..TaskTemplate::default()
        }
    }

    fn create_scenario(task: &TaskTemplate, adapter: &str) -> Option<DebugScenario> {
        create_scenario_with(&DotnetLocator::default(), task, adapter)
    }

    fn create_scenario_with(
        locator: &DotnetLocator,
        task: &TaskTemplate,
        adapter: &str,
    ) -> Option<DebugScenario> {
        futures::executor::block_on(locator.create_scenario(
            task,
            "dotnet run",
            &DebugAdapterName(adapter.to_owned().into()),
        ))
    }

    #[test]
    fn picks_the_adapter_by_target_framework() {
        assert_eq!(adapter_for(Some("net8.0"), NETCOREDBG), NETCOREDBG);
        assert_eq!(adapter_for(None, NETCOREDBG), NETCOREDBG);
        assert_eq!(adapter_for(Some("net48"), NETCOREDBG), DOTNET_DEBUGGER);
        assert_eq!(adapter_for(Some("net472"), NETCOREDBG), DOTNET_DEBUGGER);
        assert_eq!(
            adapter_for(Some("net8.0"), DOTNET_DEBUGGER),
            DOTNET_DEBUGGER
        );
    }

    #[test]
    fn locator_picks_the_adapter_for_the_project() {
        let directory = tempfile::tempdir().unwrap();
        let framework_directory = directory.path().join("Tool");
        let core_directory = directory.path().join("Api");
        std::fs::create_dir_all(&framework_directory).unwrap();
        std::fs::create_dir_all(&core_directory).unwrap();
        std::fs::write(
            framework_directory.join("Tool.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net48</TargetFramework></PropertyGroup></Project>"#,
        )
        .unwrap();
        std::fs::write(
            core_directory.join("Api.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk.Web"><PropertyGroup><TargetFrameworks>net8.0;net48</TargetFrameworks></PropertyGroup></Project>"#,
        )
        .unwrap();

        let scenario = create_scenario(
            &dotnet_run_task(&["run"], Some(&framework_directory)),
            NETCOREDBG,
        )
        .unwrap();
        assert_eq!(adapter_of(&scenario), DOTNET_DEBUGGER);
        assert_eq!(
            build_env(&scenario)
                .get(ADAPTER_VARIABLE)
                .map(String::as_str),
            Some(DOTNET_DEBUGGER)
        );

        // `--project` is relative to the task's working directory; a multi-targeting project is
        // debugged in its first framework unless the task picks one.
        let api_task = |framework: &[&str]| {
            let mut args = vec!["run", "--project", "Api/Api.csproj"];
            args.extend(framework);
            dotnet_run_task(&args, Some(directory.path()))
        };
        let scenario = create_scenario(&api_task(&[]), NETCOREDBG).unwrap();
        assert_eq!(adapter_of(&scenario), NETCOREDBG);
        let scenario = create_scenario(&api_task(&["--framework", "net48"]), NETCOREDBG).unwrap();
        assert_eq!(adapter_of(&scenario), DOTNET_DEBUGGER);

        // dotnet-debugger comes first in the C# language's debuggers, so callers pass it when the
        // user's settings name no debugger; only a setting makes it the debugger for .NET (Core).
        let scenario = create_scenario(&api_task(&[]), DOTNET_DEBUGGER).unwrap();
        assert_eq!(adapter_of(&scenario), NETCOREDBG);
        let prefers_dotnet_debugger = DotnetLocator {
            user_prefers_dotnet_debugger: AtomicBool::new(true),
        };
        let scenario =
            create_scenario_with(&prefers_dotnet_debugger, &api_task(&[]), DOTNET_DEBUGGER)
                .unwrap();
        assert_eq!(adapter_of(&scenario), DOTNET_DEBUGGER);
        // A worktree's settings can still pick netcoredbg.
        let scenario =
            create_scenario_with(&prefers_dotnet_debugger, &api_task(&[]), NETCOREDBG).unwrap();
        assert_eq!(adapter_of(&scenario), NETCOREDBG);

        // Before the task's variables are resolved, the project can't be found.
        let unresolved = dotnet_run_task(&["run"], Some(Path::new("$ZED_DIRNAME")));
        let scenario = create_scenario(&unresolved, DOTNET_DEBUGGER).unwrap();
        assert_eq!(adapter_of(&scenario), NETCOREDBG);
        assert_eq!(
            build_env(&scenario)
                .get(ADAPTER_VARIABLE)
                .map(String::as_str),
            Some(NETCOREDBG)
        );

        assert!(create_scenario(&unresolved, "codelldb").is_none());
        assert!(create_scenario(&dotnet_run_task(&["test"], None), NETCOREDBG).is_none());
    }

    #[gpui::test]
    fn follows_the_csharp_debugger_setting(cx: &mut gpui::TestAppContext) {
        use gpui::BorrowAppContext as _;

        fn set_csharp_debuggers(debuggers: &[&str], cx: &mut App) {
            let debuggers = debuggers.iter().map(|debugger| (*debugger).to_owned());
            let debuggers = debuggers.collect();
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.all_languages.languages.0.insert(
                        "C#".into(),
                        settings::LanguageSettingsContent {
                            debuggers: Some(debuggers),
                            ..Default::default()
                        },
                    );
                })
            });
        }

        let locator = cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            DotnetLocator::global(cx)
        });
        let prefers_dotnet_debugger =
            || locator.user_prefers_dotnet_debugger.load(Ordering::Relaxed);
        assert!(!prefers_dotnet_debugger());

        // The locator reads the settings when the update that changed them ends.
        cx.update(|cx| set_csharp_debuggers(&[DOTNET_DEBUGGER], cx));
        assert!(prefers_dotnet_debugger());
        cx.update(|cx| set_csharp_debuggers(&[NETCOREDBG, DOTNET_DEBUGGER], cx));
        assert!(!prefers_dotnet_debugger());
        // Each workspace registers the same locator.
        cx.update(|cx| assert!(Arc::ptr_eq(&locator, &DotnetLocator::global(cx))));
    }

    #[test]
    fn rejects_netcoredbg_for_the_net_framework() {
        let project_path = PathBuf::from("work").join("Tool").join("Tool.csproj");
        let output = |framework: &str| ProjectOutput {
            target_path: "Tool.exe".into(),
            target_framework: framework.into(),
            ..ProjectOutput::default()
        };
        let error = check_adapter(NETCOREDBG, &project_path, &output("net48")).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("Tool targets net48, which netcoredbg can't debug"),
            "{error}"
        );
        check_adapter(DOTNET_DEBUGGER, &project_path, &output("net48")).unwrap();
        check_adapter(NETCOREDBG, &project_path, &output("net8.0")).unwrap();
    }

    #[test]
    fn builds_scenarios_for_run_configurations() {
        let directory = if cfg!(windows) {
            PathBuf::from("C:\\work\\Tool")
        } else {
            PathBuf::from("/work/Tool")
        };
        let sdk_project = parse_project(
            &directory.join("Tool.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net48</TargetFramework></PropertyGroup></Project>"#,
        )
        .unwrap();
        let configurations = crate::msbuild::run_configurations(&sdk_project, true);
        let scenario = dotnet_run_scenario(&configurations[0], &sdk_project).unwrap();
        assert_eq!(adapter_of(&scenario), DOTNET_DEBUGGER);
        assert_eq!(scenario.label.as_ref(), "Debug Tool");

        let legacy_project = parse_project(
            &directory.join("Tool.csproj"),
            r#"<Project ToolsVersion="15.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
                 <PropertyGroup><OutputType>Exe</OutputType><TargetFrameworkVersion>v4.8</TargetFrameworkVersion></PropertyGroup>
                 <PropertyGroup Condition=" '$(Configuration)|$(Platform)' == 'Debug|AnyCPU' "><OutputPath>bin\Debug\</OutputPath></PropertyGroup>
               </Project>"#,
        )
        .unwrap();
        let configuration = &crate::msbuild::run_configurations(&legacy_project, true)[0];
        assert_eq!(configuration.kind, RunKind::Executable);
        assert!(dotnet_run_scenario(configuration, &legacy_project).is_err());
        let build = CommandLine {
            program: "C:\\MSBuild\\MSBuild.exe".into(),
            args: strings(&["Tool.csproj", "-p:Configuration=Debug"]),
        };
        let scenario = executable_scenario(configuration, &legacy_project, build);
        assert_eq!(adapter_of(&scenario), DOTNET_DEBUGGER);
        let output_directory = executable_output_directory(&legacy_project);
        assert_eq!(
            scenario.config,
            serde_json::json!({
                "request": "launch",
                "program": output_directory.join("Tool.exe"),
                "cwd": output_directory,
                "args": [],
                "env": {},
            })
        );
        match &scenario.build {
            Some(BuildTaskDefinition::Template {
                task_template,
                locator_name,
            }) => {
                assert_eq!(task_template.command, "C:\\MSBuild\\MSBuild.exe");
                assert_eq!(
                    task_template.args,
                    strings(&["Tool.csproj", "-p:Configuration=Debug"])
                );
                assert_eq!(*locator_name, None);
            }
            other => panic!("unexpected build: {other:?}"),
        }

        let attach = attach_scenario("Debug Site (IIS Express)".into(), 4242);
        assert_eq!(adapter_of(&attach), DOTNET_DEBUGGER);
        assert_eq!(
            attach.config,
            serde_json::json!({ "request": "attach", "processId": 4242 })
        );
    }
}
