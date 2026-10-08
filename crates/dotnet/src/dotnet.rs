mod msbuild;
mod run_configuration_picker;

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use db::kvp::KeyValueStore;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, Task, WeakEntity, Window, actions, div,
};
use project::{Project, TaskSourceKind};
use task::{RevealStrategy, SaveStrategy, TaskContext, TaskTemplate};
use ui::{
    Button, ButtonCommon, Clickable, IconButton, IconName, IconSize, LabelSize, Tooltip, h_flex,
};
use util::{
    ResultExt as _,
    rel_path::RelPath,
    shell::{ShellKind, get_system_shell},
};
use workspace::{StatusItemView, Workspace, item::ItemHandle};

use msbuild::{
    BuildVerb, CommandLine, IisExpressSite, ProjectStyle, dotnet_build_command, dotnet_run_command,
    executable_output_directory, executable_path, find_iis_express_site_name, iis_express_command,
    is_project_file, is_solution_file, msbuild_command, parse_launch_settings, parse_project,
    parse_solution, same_path,
};
pub use msbuild::{MsBuildProject, RunConfiguration, RunKind, Solution};
use run_configuration_picker::RunConfigurationPicker;

actions!(
    dotnet,
    [
        /// Builds and runs the selected .NET run configuration.
        Run,
        /// Selects the .NET project and launch profile that `dotnet: run` starts.
        SelectRunConfiguration,
        /// Builds the project of the selected .NET run configuration.
        Build,
        /// Builds the .NET solution.
        BuildSolution,
        /// Rebuilds the .NET solution from scratch.
        RebuildSolution,
        /// Deletes the .NET solution's build outputs.
        CleanSolution,
    ]
);

const RUN_CONFIGURATION_NAMESPACE: &str = "dotnet_run_configuration";
const SCAN_DEBOUNCE: Duration = Duration::from_millis(500);
const MSBUILD_NOT_FOUND: &str = "MSBuild.exe was not found. Install Visual Studio or the \
    Visual Studio Build Tools with the MSBuild component to build legacy .NET Framework projects.";

/// Registers the .NET actions on `workspace` and returns its run configuration status bar item.
pub fn register(
    workspace: &mut Workspace,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<RunConfigurationIndicator> {
    let projects = cx.new(|cx| DotnetProjects::new(workspace.project().clone(), cx));

    workspace.register_action({
        let projects = projects.clone();
        move |workspace, _: &Run, window, cx| run(&projects, workspace, window, cx)
    });
    workspace.register_action({
        let projects = projects.clone();
        move |workspace, _: &SelectRunConfiguration, window, cx| {
            RunConfigurationPicker::toggle(projects.clone(), workspace, window, cx)
        }
    });
    workspace.register_action({
        let projects = projects.clone();
        move |_, _: &Build, window, cx| build_project(&projects, window, cx)
    });
    workspace.register_action({
        let projects = projects.clone();
        move |_, _: &BuildSolution, window, cx| {
            build_solution(&projects, BuildVerb::Build, window, cx)
        }
    });
    workspace.register_action({
        let projects = projects.clone();
        move |_, _: &RebuildSolution, window, cx| {
            build_solution(&projects, BuildVerb::Rebuild, window, cx)
        }
    });
    workspace.register_action({
        let projects = projects.clone();
        move |_, _: &CleanSolution, window, cx| {
            build_solution(&projects, BuildVerb::Clean, window, cx)
        }
    });

    let workspace_handle = workspace.weak_handle();
    cx.new(|cx| RunConfigurationIndicator::new(projects, workspace_handle, cx))
}

fn run(
    projects: &Entity<DotnetProjects>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match projects.read(cx).active_run_configuration.clone() {
        Some(configuration) => run_configuration(projects, configuration, window, cx),
        None => RunConfigurationPicker::toggle(projects.clone(), workspace, window, cx),
    }
}

fn build_project(
    projects: &Entity<DotnetProjects>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let plan = projects.update(cx, |projects, cx| {
        let target = projects
            .active_run_configuration
            .as_ref()
            .map(|configuration| configuration.project_path.clone())
            .or_else(|| {
                projects
                    .default_solution()
                    .map(|solution| solution.path.clone())
            });
        match target {
            Some(target) => projects.build_plan(target, BuildVerb::Build, cx),
            None => Task::ready(Err(anyhow!("No .NET projects found in this workspace"))),
        }
    });
    spawn_plan(plan, window, cx);
}

fn build_solution(
    projects: &Entity<DotnetProjects>,
    verb: BuildVerb,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let plan = projects.update(cx, |projects, cx| {
        let target = projects
            .default_solution()
            .map(|solution| solution.path.clone());
        match target {
            Some(target) => projects.build_plan(target, verb, cx),
            None => Task::ready(Err(anyhow!("No .NET solution found in this workspace"))),
        }
    });
    spawn_plan(plan, window, cx);
}

/// The .NET solutions and projects found in a workspace's worktrees.
pub struct DotnetProjects {
    project: Entity<Project>,
    solutions: Vec<Solution>,
    projects: Vec<MsBuildProject>,
    run_configurations: Vec<RunConfiguration>,
    active_run_configuration: Option<RunConfiguration>,
    msbuild: Option<PathBuf>,
    scan_task: Task<()>,
    _subscription: Subscription,
}

impl DotnetProjects {
    fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&project, |this, _, event, cx| match event {
            project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                this.schedule_scan(cx)
            }
            project::Event::WorktreeUpdatedEntries(_, changes) => {
                if changes
                    .iter()
                    .any(|(path, _, _)| affects_project_model(path))
                {
                    this.schedule_scan(cx)
                }
            }
            _ => {}
        });
        let mut this = Self {
            project,
            solutions: Vec::new(),
            projects: Vec::new(),
            run_configurations: Vec::new(),
            active_run_configuration: None,
            msbuild: None,
            scan_task: Task::ready(()),
            _subscription: subscription,
        };
        this.schedule_scan(cx);
        this
    }

    pub fn run_configurations(&self) -> &[RunConfiguration] {
        &self.run_configurations
    }

    pub fn active_run_configuration(&self) -> Option<&RunConfiguration> {
        self.active_run_configuration.as_ref()
    }

    pub fn select_run_configuration(
        &mut self,
        configuration: RunConfiguration,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.persistence_key(cx)
            && let Some(json) = serde_json::to_string(&configuration).log_err()
        {
            let store = KeyValueStore::global(cx);
            db::write_and_log(cx, move || async move {
                store
                    .scoped(RUN_CONFIGURATION_NAMESPACE)
                    .write(key, json)
                    .await
            });
        }
        self.active_run_configuration = Some(configuration);
        cx.notify();
    }

    fn schedule_scan(&mut self, cx: &mut Context<Self>) {
        self.scan_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SCAN_DEBOUNCE).await;
            let Some((fs, snapshots)) = this
                .update(cx, |this, cx| {
                    let project = this.project.read(cx);
                    // Projects are read straight from disk, which only works for local projects.
                    if !project.is_local() {
                        return None;
                    }
                    let snapshots = project
                        .visible_worktrees(cx)
                        .map(|worktree| worktree.read(cx).snapshot())
                        .collect::<Vec<_>>();
                    Some((project.fs().clone(), snapshots))
                })
                .ok()
                .flatten()
            else {
                return;
            };

            let (solutions, projects) = cx
                .background_spawn(async move {
                    let paths = snapshots
                        .iter()
                        .flat_map(|snapshot| {
                            snapshot
                                .files(false, 0)
                                .filter(|entry| {
                                    let path = entry.path.as_std_path();
                                    is_project_file(path) || is_solution_file(path)
                                })
                                .map(|entry| snapshot.absolutize(&entry.path))
                        })
                        .collect::<Vec<_>>();
                    load_project_model(fs.as_ref(), paths).await
                })
                .await;

            this.update(cx, |this, cx| {
                this.set_project_model(solutions, projects, cx)
            })
            .log_err();
        });
    }

    fn set_project_model(
        &mut self,
        solutions: Vec<Solution>,
        projects: Vec<MsBuildProject>,
        cx: &mut Context<Self>,
    ) {
        let mut run_configurations = projects
            .iter()
            .flat_map(|project| msbuild::run_configurations(project, cfg!(windows)))
            .collect::<Vec<_>>();
        run_configurations.sort_by_cached_key(|configuration| configuration.label().to_lowercase());

        self.solutions = solutions;
        self.projects = projects;
        self.run_configurations = run_configurations;

        let active_is_valid = self
            .active_run_configuration
            .as_ref()
            .is_some_and(|active| self.run_configurations.contains(active));
        if !active_is_valid {
            self.active_run_configuration = self
                .saved_run_configuration(cx)
                .filter(|saved| self.run_configurations.contains(saved))
                .or_else(|| self.run_configurations.first().cloned());
        }
        cx.notify();
    }

    /// Formats `path` relative to its worktree, prefixed with the worktree's name.
    pub(crate) fn display_path(&self, path: &Path, cx: &App) -> String {
        let project = self.project.read(cx);
        match project.find_worktree(path, cx) {
            Some((worktree, relative_path)) => {
                let path_style = project.path_style(cx);
                let worktree = worktree.read(cx);
                let root_name = worktree.root_name().display(path_style);
                if relative_path.is_empty() {
                    root_name.into_owned()
                } else {
                    format!(
                        "{root_name}{}{}",
                        path_style.primary_separator(),
                        relative_path.display(path_style)
                    )
                }
            }
            None => path.to_string_lossy().into_owned(),
        }
    }

    fn persistence_key(&self, cx: &App) -> Option<String> {
        let mut roots = self
            .project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if roots.is_empty() {
            return None;
        }
        roots.sort();
        Some(roots.join("\n"))
    }

    fn saved_run_configuration(&self, cx: &App) -> Option<RunConfiguration> {
        let key = self.persistence_key(cx)?;
        let json = KeyValueStore::global(cx)
            .scoped(RUN_CONFIGURATION_NAMESPACE)
            .read(&key)
            .log_err()
            .flatten()?;
        serde_json::from_str(&json).log_err()
    }

    fn project(&self, path: &Path) -> Option<&MsBuildProject> {
        self.projects
            .iter()
            .find(|project| same_path(&project.path, path))
    }

    fn solution_for(&self, project_path: &Path) -> Option<&Solution> {
        self.solutions
            .iter()
            .find(|solution| solution.contains(project_path))
    }

    /// The solution containing the selected run configuration's project, or else the solution
    /// closest to a worktree root.
    fn default_solution(&self) -> Option<&Solution> {
        self.active_run_configuration
            .as_ref()
            .and_then(|configuration| self.solution_for(&configuration.project_path))
            .or_else(|| {
                self.solutions
                    .iter()
                    .min_by_key(|solution| solution.path.components().count())
            })
    }

    fn msbuild_path(&mut self, cx: &mut Context<Self>) -> Task<Result<PathBuf>> {
        if let Some(path) = self.msbuild.clone() {
            return Task::ready(Ok(path));
        }
        cx.spawn(async move |this, cx| {
            let path = find_msbuild().await.context(MSBUILD_NOT_FOUND)?;
            this.update(cx, |this, _| this.msbuild = Some(path.clone()))?;
            Ok(path)
        })
    }

    fn build_plan(
        &mut self,
        target: PathBuf,
        verb: BuildVerb,
        cx: &mut Context<Self>,
    ) -> Task<Result<CommandPlan>> {
        let (uses_msbuild, solution_directory) = if is_solution_file(&target) {
            let uses_msbuild = self
                .solutions
                .iter()
                .find(|solution| same_path(&solution.path, &target))
                .is_some_and(|solution| {
                    self.projects.iter().any(|project| {
                        project.style == ProjectStyle::Legacy && solution.contains(&project.path)
                    })
                });
            (uses_msbuild, None)
        } else {
            let uses_msbuild = self
                .project(&target)
                .is_some_and(|project| project.style == ProjectStyle::Legacy);
            let solution_directory = self
                .solution_for(&target)
                .map(|solution| solution.directory().to_path_buf());
            (uses_msbuild, solution_directory)
        };
        let msbuild = uses_msbuild.then(|| self.msbuild_path(cx));
        let file_name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let label = format!("{} {file_name}", verb.label());
        let cwd = target.parent().map(Path::to_path_buf).unwrap_or_default();

        cx.spawn(async move |_, _| {
            let command = match msbuild {
                Some(msbuild) => msbuild_command(
                    &msbuild.await?,
                    &target,
                    solution_directory.as_deref(),
                    verb,
                ),
                None => dotnet_build_command(&target, verb),
            };
            Ok(CommandPlan {
                label,
                source_path: target,
                steps: vec![command],
                cwd,
            })
        })
    }

    fn run_plan(
        &mut self,
        configuration: RunConfiguration,
        cx: &mut Context<Self>,
    ) -> Task<Result<CommandPlan>> {
        let Some(project) = self.project(&configuration.project_path).cloned() else {
            return Task::ready(Err(anyhow!(
                "{} is no longer part of this workspace",
                configuration.project_path.display()
            )));
        };
        let solution = self.solution_for(&project.path).cloned();
        let fs = self.project.read(cx).fs().clone();
        let msbuild = (project.style == ProjectStyle::Legacy).then(|| self.msbuild_path(cx));
        let label = format!("Run {}", configuration.label());

        cx.spawn(async move |_, _| {
            let solution_directory = solution.as_ref().map(Solution::directory);
            let (steps, cwd) = match &configuration.kind {
                RunKind::DotnetRun {
                    launch_profile,
                    framework,
                } => (
                    vec![dotnet_run_command(
                        &project,
                        launch_profile.as_deref(),
                        framework.as_deref(),
                    )],
                    project.directory().to_path_buf(),
                ),
                RunKind::Executable => {
                    let msbuild = msbuild.context(MSBUILD_NOT_FOUND)?.await?;
                    // Like Visual Studio, start the program from its output directory. It has to
                    // exist before the terminal can start there, ahead of the first build.
                    let output_directory = executable_output_directory(&project);
                    fs.create_dir(&output_directory).await?;
                    (
                        vec![
                            msbuild_command(
                                &msbuild,
                                &project.path,
                                solution_directory,
                                BuildVerb::Build,
                            ),
                            CommandLine {
                                program: executable_path(&project).to_string_lossy().into_owned(),
                                args: Vec::new(),
                            },
                        ],
                        output_directory,
                    )
                }
                RunKind::IisExpress => {
                    let msbuild = msbuild.context(MSBUILD_NOT_FOUND)?.await?;
                    let iis_express = find_iis_express(fs.as_ref()).await.context(
                        "IIS Express was not found. Install it from Visual Studio or \
                         https://www.microsoft.com/download/details.aspx?id=48264",
                    )?;
                    let site =
                        find_iis_express_site(fs.as_ref(), solution.as_ref(), &project).await;
                    (
                        vec![
                            msbuild_command(
                                &msbuild,
                                &project.path,
                                solution_directory,
                                BuildVerb::Build,
                            ),
                            iis_express_command(&iis_express, &project, site.as_ref()),
                        ],
                        project.directory().to_path_buf(),
                    )
                }
            };
            Ok(CommandPlan {
                label,
                source_path: project.path.clone(),
                steps,
                cwd,
            })
        })
    }
}

fn affects_project_model(path: &RelPath) -> bool {
    let path = path.as_std_path();
    is_project_file(path)
        || is_solution_file(path)
        || path
            .file_name()
            .is_some_and(|file_name| file_name == "launchSettings.json")
}

async fn load_project_model(
    fs: &dyn Fs,
    paths: Vec<PathBuf>,
) -> (Vec<Solution>, Vec<MsBuildProject>) {
    let mut solutions = Vec::new();
    let mut projects = Vec::new();
    for path in paths {
        let Some(contents) = fs
            .load(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))
            .log_err()
        else {
            continue;
        };
        if is_solution_file(&path) {
            if let Some(solution) = parse_solution(&path, &contents)
                .with_context(|| format!("parsing {}", path.display()))
                .log_err()
            {
                solutions.push(solution);
            }
        } else if let Some(mut project) = parse_project(&path, &contents)
            .with_context(|| format!("parsing {}", path.display()))
            .log_err()
        {
            project.launch_profiles = load_launch_profiles(fs, &project).await;
            projects.push(project);
        }
    }
    (solutions, projects)
}

async fn load_launch_profiles(fs: &dyn Fs, project: &MsBuildProject) -> Vec<String> {
    // Visual Basic projects keep their launch settings under "My Project".
    for directory in ["Properties", "My Project"] {
        let path = project
            .directory()
            .join(directory)
            .join("launchSettings.json");
        if fs.is_file(&path).await {
            return fs
                .load(&path)
                .await
                .and_then(|contents| parse_launch_settings(&contents))
                .with_context(|| format!("reading {}", path.display()))
                .log_err()
                .unwrap_or_default();
        }
    }
    Vec::new()
}

async fn find_msbuild() -> Option<PathBuf> {
    if cfg!(windows)
        && let Some(program_files) = std::env::var_os("ProgramFiles(x86)")
    {
        let vswhere = PathBuf::from(program_files)
            .join("Microsoft Visual Studio")
            .join("Installer")
            .join("vswhere.exe");
        // `-products *` is needed to also find standalone Build Tools installations.
        let output = util::command::new_command(&vswhere)
            .args([
                "-latest",
                "-prerelease",
                "-products",
                "*",
                "-requires",
                "Microsoft.Component.MSBuild",
                "-find",
                "MSBuild\\**\\Bin\\MSBuild.exe",
            ])
            .output()
            .await
            .with_context(|| format!("running {}", vswhere.display()))
            .log_err();
        if let Some(output) = output
            && output.status.success()
            && let Some(path) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
        {
            return Some(PathBuf::from(path));
        }
    }
    which::which("msbuild").ok()
}

async fn find_iis_express(fs: &dyn Fs) -> Option<PathBuf> {
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(program_files) = std::env::var_os(variable) {
            let path = PathBuf::from(program_files)
                .join("IIS Express")
                .join("iisexpress.exe");
            if fs.is_file(&path).await {
                return Some(path);
            }
        }
    }
    None
}

/// Looks for the IIS Express site Visual Studio (`.vs`) or Rider (`.idea`) configured for
/// `project`, so that it runs with the same bindings, including HTTPS.
async fn find_iis_express_site(
    fs: &dyn Fs,
    solution: Option<&Solution>,
    project: &MsBuildProject,
) -> Option<IisExpressSite> {
    let solution_directory = solution?.directory();
    let mut config_paths = Vec::new();
    for settings_directory in [".vs", ".idea"] {
        let settings_directory = solution_directory.join(settings_directory);
        config_paths.push(
            settings_directory
                .join("config")
                .join("applicationhost.config"),
        );
        if let Ok(mut children) = fs.read_dir(&settings_directory).await {
            while let Some(child) = children.next().await {
                if let Ok(child) = child {
                    config_paths.push(child.join("config").join("applicationhost.config"));
                }
            }
        }
    }

    for config_path in config_paths {
        if !fs.is_file(&config_path).await {
            continue;
        }
        let site_name = fs
            .load(&config_path)
            .await
            .and_then(|config| find_iis_express_site_name(&config, project))
            .with_context(|| format!("reading {}", config_path.display()))
            .log_err()
            .flatten();
        if let Some(name) = site_name {
            return Some(IisExpressSite { config_path, name });
        }
    }
    None
}

struct CommandPlan {
    label: String,
    source_path: PathBuf,
    steps: Vec<CommandLine>,
    cwd: PathBuf,
}

fn run_configuration(
    projects: &Entity<DotnetProjects>,
    configuration: RunConfiguration,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let plan = projects.update(cx, |projects, cx| projects.run_plan(configuration, cx));
    spawn_plan(plan, window, cx);
}

fn spawn_plan(plan: Task<Result<CommandPlan>>, window: &mut Window, cx: &mut Context<Workspace>) {
    cx.spawn_in(window, async move |workspace, cx| {
        let plan = plan.await;
        workspace
            .update_in(cx, |workspace, window, cx| match plan {
                Ok(plan) => schedule_plan(plan, workspace, window, cx),
                Err(error) => workspace.show_error(error, cx),
            })
            .log_err();
    })
    .detach();
}

/// Runs the plan's steps as a single task, so that rerunning the task from the terminal or
/// `task: rerun` repeats the build as well.
fn schedule_plan(
    plan: CommandPlan,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // Tasks are passed to the system shell verbatim, so arguments must be quoted for it.
    let shell_kind = ShellKind::new(get_system_shell(), cfg!(windows));
    let template = TaskTemplate {
        label: plan.label,
        command: shell_command_line(&plan.steps, shell_kind),
        cwd: Some(plan.cwd.to_string_lossy().into_owned()),
        // Rerunning replaces the previous run, which also stops a server that is still serving.
        allow_concurrent_runs: true,
        reveal: RevealStrategy::Always,
        show_summary: true,
        show_command: true,
        save: SaveStrategy::All,
        ..TaskTemplate::default()
    };
    let source_kind = TaskSourceKind::AbsPath {
        id_base: Cow::Borrowed("dotnet"),
        abs_path: plan.source_path,
    };
    workspace.schedule_task(
        source_kind,
        &template,
        &TaskContext::default(),
        false,
        window,
        cx,
    );
}

fn shell_command_line(steps: &[CommandLine], shell_kind: ShellKind) -> String {
    let commands = steps.iter().map(|step| {
        let program = shell_kind
            .try_quote_prefix_aware(&step.program)
            .unwrap_or(Cow::Borrowed(&step.program));
        std::iter::once(program)
            .chain(step.args.iter().map(|arg| {
                shell_kind
                    .try_quote(arg)
                    .unwrap_or(Cow::Borrowed(arg.as_str()))
            }))
            .collect::<Vec<_>>()
            .join(" ")
    });
    let mut command_line = String::new();
    for command in commands {
        if command_line.is_empty() {
            command_line = command;
        } else if shell_kind == ShellKind::PowerShell {
            // Windows PowerShell 5 has no `&&`.
            command_line = format!("{command_line}; if ($LASTEXITCODE -eq 0) {{ {command} }}");
        } else {
            let separator = shell_kind.sequential_and_commands_separator();
            command_line = format!("{command_line} {separator} {command}");
        }
    }
    command_line
}

/// Shows the selected run configuration in the status bar, with a button to run it.
pub struct RunConfigurationIndicator {
    projects: Entity<DotnetProjects>,
    workspace: WeakEntity<Workspace>,
    _observe_projects: Subscription,
}

impl RunConfigurationIndicator {
    fn new(
        projects: Entity<DotnetProjects>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe_projects = cx.observe(&projects, |_, _, cx| cx.notify());
        Self {
            projects,
            workspace,
            _observe_projects: observe_projects,
        }
    }
}

impl Render for RunConfigurationIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(configuration) = self.projects.read(cx).active_run_configuration() else {
            return div().hidden();
        };

        div().child(
            h_flex()
                .child(
                    Button::new("dotnet-run-configuration", configuration.label())
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let projects = this.projects.clone();
                            this.workspace
                                .update(cx, |workspace, cx| {
                                    RunConfigurationPicker::toggle(projects, workspace, window, cx)
                                })
                                .log_err();
                        }))
                        .tooltip(Tooltip::for_action_title(
                            "Select Run Configuration",
                            &SelectRunConfiguration,
                        )),
                )
                .child(
                    IconButton::new("dotnet-run", IconName::PlayOutlined)
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let projects = this.projects.clone();
                            this.workspace
                                .update(cx, |workspace, cx| run(&projects, workspace, window, cx))
                                .log_err();
                        }))
                        .tooltip(Tooltip::for_action_title("Run", &Run)),
                ),
        )
    }
}

impl StatusItemView for RunConfigurationIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // Only shown in workspaces that contain runnable .NET projects.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use pretty_assertions::assert_eq;
    use project::task_store::TaskStore;
    use serde_json::json;
    use task::ResolvedTask;
    use util::path;
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    async fn discovers_and_runs_configurations(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            TaskStore::init(None);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "Sample.sln": "\u{feff}\nMicrosoft Visual Studio Solution File, Format Version 12.00\n\
                    Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Api\", \"src\\Api\\Api.csproj\", \"{4CFE6531-67B4-4C56-9DEE-79F74621AE96}\"\nEndProject\n\
                    Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Tool\", \"src\\Tool\\Tool.csproj\", \"{BFB0C338-BA0A-4F4C-994B-9F9F02376D49}\"\nEndProject\n",
                "src": {
                    "Api": {
                        "Api.csproj": r#"<Project Sdk="Microsoft.NET.Sdk.Web"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"#,
                        "Properties": {
                            "launchSettings.json": r#"{"profiles": {"http": {"commandName": "Project"}, "https": {"commandName": "Project"}, "IIS Express": {"commandName": "IISExpress"}}}"#,
                        },
                    },
                    "Tool": {
                        "Tool.csproj": r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"#,
                    },
                    "Core": {
                        "Core.csproj": r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>netstandard2.0</TargetFramework></PropertyGroup></Project>"#,
                    },
                },
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        // In the app, actions are registered before the workspace first renders, which is when
        // they get attached to the window.
        let indicator = workspace.update_in(cx, |workspace, window, cx| {
            let indicator = register(workspace, window, cx);
            cx.notify();
            indicator
        });
        let projects = indicator.read_with(cx, |indicator, _| indicator.projects.clone());
        cx.executor().advance_clock(SCAN_DEBOUNCE);
        cx.run_until_parked();

        let labels = |cx: &mut VisualTestContext| {
            projects.read_with(cx, |projects, _| {
                projects
                    .run_configurations()
                    .iter()
                    .map(RunConfiguration::label)
                    .collect::<Vec<_>>()
            })
        };
        let active_label = |cx: &mut VisualTestContext| {
            projects.read_with(cx, |projects, _| {
                projects
                    .active_run_configuration()
                    .map(RunConfiguration::label)
            })
        };
        let last_task = |cx: &mut VisualTestContext| -> Option<ResolvedTask> {
            project.read_with(cx, |project, cx| {
                let inventory = project.task_store().read(cx).task_inventory()?.read(cx);
                inventory.last_scheduled_task(None).map(|(_, task)| task)
            })
        };

        assert_eq!(labels(cx), vec!["Api (http)", "Api (https)", "Tool"]);
        assert_eq!(active_label(cx).as_deref(), Some("Api (http)"));

        cx.dispatch_action(Run);
        cx.run_until_parked();
        let task = last_task(cx).expect("running should schedule a task");
        assert_eq!(task.resolved_label, "Run Api (http)");
        let command = task.resolved.command.clone().unwrap_or_default();
        assert!(
            command.starts_with("dotnet run --project ")
                && command.ends_with("--launch-profile http"),
            "unexpected command: {command}"
        );
        assert_eq!(
            task.resolved.cwd.as_deref(),
            Some(Path::new(path!("/root/src/Api")))
        );

        // Secondary confirmation selects a configuration without running it.
        cx.dispatch_action(SelectRunConfiguration);
        cx.run_until_parked();
        cx.simulate_input("tool");
        cx.dispatch_action(menu::SecondaryConfirm);
        cx.run_until_parked();
        assert_eq!(active_label(cx).as_deref(), Some("Tool"));
        assert_eq!(
            last_task(cx).map(|task| task.resolved_label),
            Some("Run Api (http)".to_string())
        );

        cx.dispatch_action(SelectRunConfiguration);
        cx.run_until_parked();
        cx.simulate_input("https");
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
        assert_eq!(active_label(cx).as_deref(), Some("Api (https)"));
        assert_eq!(
            last_task(cx).map(|task| task.resolved_label),
            Some("Run Api (https)".to_string())
        );

        cx.dispatch_action(BuildSolution);
        cx.run_until_parked();
        let task = last_task(cx).expect("building should schedule a task");
        assert_eq!(task.resolved_label, "Build Sample.sln");
        let command = task.resolved.command.unwrap_or_default();
        assert!(
            command.starts_with("dotnet build ") && command.contains("Sample.sln"),
            "unexpected command: {command}"
        );
    }

    #[test]
    fn joins_steps_for_each_shell() {
        let steps = vec![
            CommandLine {
                program: "C:\\Program Files\\MSBuild.exe".into(),
                args: vec!["C:\\work\\My App\\App.csproj".into(), "-nologo".into()],
            },
            CommandLine {
                program: "C:\\Program Files\\IIS Express\\iisexpress.exe".into(),
                args: vec!["/site:App(1)".into()],
            },
        ];
        assert_eq!(
            shell_command_line(&steps, ShellKind::Pwsh),
            "&'C:\\Program Files\\MSBuild.exe' 'C:\\work\\My App\\App.csproj' -nologo && \
             &'C:\\Program Files\\IIS Express\\iisexpress.exe' '/site:App(1)'"
        );
        assert_eq!(
            shell_command_line(&steps, ShellKind::PowerShell),
            "&'C:\\Program Files\\MSBuild.exe' 'C:\\work\\My App\\App.csproj' -nologo; \
             if ($LASTEXITCODE -eq 0) { &'C:\\Program Files\\IIS Express\\iisexpress.exe' '/site:App(1)' }"
        );

        let steps = vec![CommandLine {
            program: "dotnet".into(),
            args: vec![
                "run".into(),
                "--project".into(),
                "/work/My App/App.csproj".into(),
            ],
        }];
        assert_eq!(
            shell_command_line(&steps, ShellKind::Posix),
            "dotnet run --project '/work/My App/App.csproj'"
        );
    }
}
