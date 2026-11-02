use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use dotnet::{
    BuildVerb, DotnetProjects, MsBuildProject, OutputType, ProjectStyle, RunConfiguration, Solution,
};
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClickEvent, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent, PathPromptOptions, Pixels,
    Point, Render, SharedString, Subscription, Task, WeakEntity, Window, actions, anchored,
    deferred, px,
};
use project::DirectoryLister;
use ui::{ContextMenu, Icon, IconName, IconSize, Label, LabelSize, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    MultiWorkspace, OpenMode, OpenOptions, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::nuget_panel::NuGetPanel;

actions!(
    solution_explorer,
    [
        /// Toggles focus on the .NET Solution Explorer.
        ToggleFocus,
        /// Picks a .sln or .slnx file, opens its folder and shows the Solution Explorer.
        OpenSolution,
    ]
);

const PANEL_KEY: &str = "SolutionExplorer";
/// How long to wait for a newly opened workspace to add its Solution Explorer.
const PANEL_WAIT_ATTEMPTS: usize = 50;
const PANEL_WAIT_INTERVAL: Duration = Duration::from_millis(100);

/// A Visual Studio-style tree of the workspace's .NET solutions, projects and their references,
/// with build, run, debug and test commands per project.
pub struct SolutionExplorer {
    workspace: WeakEntity<Workspace>,
    projects: Entity<DotnetProjects>,
    focus_handle: FocusHandle,
    position: DockPosition,
    expanded: HashMap<NodeKey, bool>,
    selected: Option<NodeKey>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    _observe_projects: Subscription,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum NodeKey {
    Solution(PathBuf),
    /// Projects that no solution in the workspace lists.
    OtherProjects,
    /// A project, under the solution (or `OtherProjects`) it's shown in.
    Project(Option<PathBuf>, PathBuf),
    ProjectReferences(PathBuf),
    Packages(PathBuf),
    ProjectReference(PathBuf, PathBuf),
    Package(PathBuf, String),
}

impl NodeKey {
    fn expanded_by_default(&self) -> bool {
        matches!(self, NodeKey::Solution(_) | NodeKey::OtherProjects)
    }
}

struct Row {
    key: NodeKey,
    depth: usize,
    icon: IconName,
    label: SharedString,
    detail: Option<SharedString>,
    /// `Some(expanded)` for rows with children.
    expanded: Option<bool>,
}

impl SolutionExplorer {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                let projects = dotnet::workspace_projects(workspace, cx)
                    .context("the .NET project model is not registered")?;
                workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                    workspace.toggle_panel_focus::<SolutionExplorer>(window, cx);
                });
                workspace.register_action(|workspace, _: &OpenSolution, window, cx| {
                    open_solution(workspace, window, cx);
                });
                let workspace_handle = cx.entity().downgrade();
                Ok(cx.new(|cx| Self::new(workspace_handle, projects, window, cx)))
            })?
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        projects: Entity<DotnetProjects>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe_projects = cx.observe(&projects, |_, _, cx| cx.notify());
        Self {
            workspace,
            projects,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            expanded: HashMap::default(),
            selected: None,
            context_menu: None,
            _observe_projects: observe_projects,
        }
    }

    fn is_expanded(&self, key: &NodeKey) -> bool {
        self.expanded
            .get(key)
            .copied()
            .unwrap_or_else(|| key.expanded_by_default())
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let model = self.projects.read(cx);
        let mut rows = Vec::new();
        let mut shown_projects = Vec::new();

        let mut solutions = model.solutions().iter().collect::<Vec<_>>();
        solutions.sort_by_key(|solution| solution.path.components().count());
        for solution in solutions {
            let key = NodeKey::Solution(solution.path.clone());
            let expanded = self.is_expanded(&key);
            let solution_projects = model
                .projects()
                .iter()
                .filter(|project| solution.contains(&project.path))
                .collect::<Vec<_>>();
            rows.push(Row {
                depth: 0,
                icon: IconName::Blocks,
                label: solution_file_name(solution).into(),
                detail: Some(project_count(solution_projects.len()).into()),
                expanded: Some(expanded),
                key,
            });
            shown_projects.extend(solution_projects.iter().map(|project| project.path.clone()));
            if expanded {
                for project in solution_projects {
                    self.push_project(&mut rows, model, Some(solution), project, 1);
                }
            }
        }

        let other_projects = model
            .projects()
            .iter()
            .filter(|project| !shown_projects.contains(&project.path))
            .collect::<Vec<_>>();
        if !other_projects.is_empty() {
            let key = NodeKey::OtherProjects;
            let expanded = self.is_expanded(&key);
            rows.push(Row {
                depth: 0,
                icon: IconName::Folder,
                label: "Projects without a solution".into(),
                detail: Some(project_count(other_projects.len()).into()),
                expanded: Some(expanded),
                key,
            });
            if expanded {
                for project in other_projects {
                    self.push_project(&mut rows, model, None, project, 1);
                }
            }
        }
        rows
    }

    fn push_project(
        &self,
        rows: &mut Vec<Row>,
        model: &DotnetProjects,
        solution: Option<&Solution>,
        project: &MsBuildProject,
        depth: usize,
    ) {
        let key = NodeKey::Project(
            solution.map(|solution| solution.path.clone()),
            project.path.clone(),
        );
        let expanded = self.is_expanded(&key);
        let is_startup = model
            .active_run_configuration()
            .is_some_and(|configuration| configuration.project_path == project.path);
        let mut details = project.target_frameworks.clone();
        if project.style == ProjectStyle::Legacy {
            details.push("legacy".into());
        }
        if is_startup {
            details.push("startup".into());
        }
        rows.push(Row {
            depth,
            icon: project_icon(project),
            label: project.name.clone().into(),
            detail: (!details.is_empty()).then(|| details.join(", ").into()),
            expanded: Some(expanded),
            key,
        });
        if !expanded {
            return;
        }

        if !project.project_references.is_empty() {
            let key = NodeKey::ProjectReferences(project.path.clone());
            let references_expanded = self.is_expanded(&key);
            rows.push(Row {
                depth: depth + 1,
                icon: IconName::Link,
                label: "Projects".into(),
                detail: Some(project.project_references.len().to_string().into()),
                expanded: Some(references_expanded),
                key,
            });
            if references_expanded {
                for reference in &project.project_references {
                    let name = model
                        .projects()
                        .iter()
                        .find(|candidate| dotnet::same_path(&candidate.path, reference))
                        .map(|candidate| candidate.name.clone())
                        .unwrap_or_else(|| file_stem(reference));
                    rows.push(Row {
                        key: NodeKey::ProjectReference(project.path.clone(), reference.clone()),
                        depth: depth + 2,
                        icon: IconName::FileCode,
                        label: name.into(),
                        detail: None,
                        expanded: None,
                    });
                }
            }
        }

        let key = NodeKey::Packages(project.path.clone());
        let packages_expanded = self.is_expanded(&key);
        rows.push(Row {
            depth: depth + 1,
            icon: IconName::Box,
            label: if project.uses_packages_config {
                "Packages (packages.config)".into()
            } else {
                "Packages".into()
            },
            detail: Some(project.package_references.len().to_string().into()),
            expanded: Some(packages_expanded),
            key,
        });
        if packages_expanded {
            for package in &project.package_references {
                rows.push(Row {
                    key: NodeKey::Package(project.path.clone(), package.id.clone()),
                    depth: depth + 2,
                    icon: IconName::BoxOpen,
                    label: package.id.clone().into(),
                    detail: Some(
                        package
                            .version
                            .clone()
                            .unwrap_or_else(|| "central".into())
                            .into(),
                    ),
                    expanded: None,
                });
            }
        }
    }

    fn toggle(&mut self, key: &NodeKey, cx: &mut Context<Self>) {
        let expanded = self.is_expanded(key);
        self.expanded.insert(key.clone(), !expanded);
        cx.notify();
    }

    fn activate(
        &mut self,
        key: &NodeKey,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(key.clone());
        match key {
            NodeKey::Solution(path) | NodeKey::Project(_, path) if click_count > 1 => {
                self.open_file(path.clone(), window, cx)
            }
            NodeKey::ProjectReference(_, path) => self.open_file(path.clone(), window, cx),
            NodeKey::Package(project_path, package_id) => {
                self.manage_packages(project_path.clone(), Some(package_id.clone()), window, cx)
            }
            key => self.toggle(key, cx),
        }
        cx.notify();
    }

    fn open_file(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(path, OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn manage_packages(
        &self,
        project_path: PathBuf,
        package_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        // Focusing the NuGet panel deactivates this panel when they share a dock, which would
        // update this panel while it's still being updated.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    let Some(panel) = workspace.panel::<NuGetPanel>(cx) else {
                        return;
                    };
                    panel.update(cx, |panel, cx| {
                        panel.show_project(project_path, package_id, window, cx)
                    });
                    workspace.focus_panel::<NuGetPanel>(window, cx);
                })
                .log_err();
        });
    }

    fn deploy_context_menu(
        &mut self,
        key: NodeKey,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(key.clone());
        let model = self.projects.read(cx);
        let projects = self.projects.clone();
        let workspace = self.workspace.clone();
        let this = cx.entity().downgrade();

        let menu = match &key {
            NodeKey::Solution(path) => {
                let path = path.clone();
                // A solution opened on its own, or found outside the open folders, can have its
                // folder added so the file finder, search and project panel see its files.
                let add_solution_folder = path
                    .parent()
                    .filter(|directory| {
                        workspace.upgrade().is_some_and(|workspace| {
                            workspace
                                .read(cx)
                                .project()
                                .read(cx)
                                .find_worktree(directory, cx)
                                .is_none()
                        })
                    })
                    .map(|directory| {
                        let workspace = workspace.clone();
                        let directory = directory.to_path_buf();
                        let solution_path = path.clone();
                        move |_: &mut Window, cx: &mut App| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    add_solution_folder(workspace, &solution_path, &directory, cx)
                                })
                                .log_err();
                        }
                    });
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let build = |verb: BuildVerb| {
                        let projects = projects.clone();
                        let workspace = workspace.clone();
                        let path = path.clone();
                        move |window: &mut Window, cx: &mut App| {
                            workspace
                                .update(cx, |_, cx| {
                                    dotnet::build_target(&projects, path.clone(), verb, window, cx)
                                })
                                .log_err();
                        }
                    };
                    let open = {
                        let this = this.clone();
                        let path = path.clone();
                        move |window: &mut Window, cx: &mut App| {
                            this.update(cx, |this, cx| this.open_file(path.clone(), window, cx))
                                .log_err();
                        }
                    };
                    let menu = menu
                        .entry("Build Solution", None, build(BuildVerb::Build))
                        .entry("Rebuild Solution", None, build(BuildVerb::Rebuild))
                        .entry("Clean Solution", None, build(BuildVerb::Clean))
                        .separator()
                        .entry("Open Solution File", None, open);
                    match add_solution_folder.clone() {
                        Some(add_solution_folder) => menu.entry(
                            "Add Solution Folder to Workspace",
                            None,
                            add_solution_folder,
                        ),
                        None => menu,
                    }
                })
            }
            NodeKey::Project(_, path) => {
                let Some(project) = model
                    .projects()
                    .iter()
                    .find(|project| project.path == *path)
                    .cloned()
                else {
                    return;
                };
                let configurations = model
                    .run_configurations()
                    .iter()
                    .filter(|configuration| configuration.project_path == project.path)
                    .cloned()
                    .collect::<Vec<_>>();
                ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let build = |verb: BuildVerb| {
                        let projects = projects.clone();
                        let workspace = workspace.clone();
                        let path = project.path.clone();
                        move |window: &mut Window, cx: &mut App| {
                            workspace
                                .update(cx, |_, cx| {
                                    dotnet::build_target(&projects, path.clone(), verb, window, cx)
                                })
                                .log_err();
                        }
                    };
                    let start = |configuration: RunConfiguration, debug: bool| {
                        let projects = projects.clone();
                        let workspace = workspace.clone();
                        move |window: &mut Window, cx: &mut App| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    dotnet::start_run_configuration(
                                        &projects,
                                        configuration.clone(),
                                        debug,
                                        workspace,
                                        window,
                                        cx,
                                    )
                                })
                                .log_err();
                        }
                    };

                    menu = menu
                        .entry("Build", None, build(BuildVerb::Build))
                        .entry("Rebuild", None, build(BuildVerb::Rebuild))
                        .entry("Clean", None, build(BuildVerb::Clean));
                    if project.is_test_project {
                        let projects = projects.clone();
                        let workspace = workspace.clone();
                        let path = project.path.clone();
                        menu = menu.entry("Run Tests", None, move |window, cx| {
                            workspace
                                .update(cx, |_, cx| {
                                    dotnet::test_project(&projects, path.clone(), window, cx)
                                })
                                .log_err();
                        });
                    }
                    if !configurations.is_empty() {
                        menu = menu.separator();
                        for configuration in &configurations {
                            menu = menu.entry(
                                format!("Run {}", configuration.label()),
                                None,
                                start(configuration.clone(), false),
                            );
                            if configuration.can_debug() {
                                menu = menu.entry(
                                    format!("Debug {}", configuration.label()),
                                    None,
                                    start(configuration.clone(), true),
                                );
                            }
                        }
                        if let Some(configuration) = configurations.first().cloned() {
                            let projects = projects.clone();
                            menu = menu.entry("Set as Startup Project", None, move |_, cx| {
                                projects.update(cx, |projects, cx| {
                                    projects.select_run_configuration(configuration.clone(), cx)
                                });
                            });
                        }
                    }
                    let manage = {
                        let this = this.clone();
                        let path = project.path.clone();
                        move |window: &mut Window, cx: &mut App| {
                            this.update(cx, |this, cx| {
                                this.manage_packages(path.clone(), None, window, cx)
                            })
                            .log_err();
                        }
                    };
                    let open = {
                        let this = this.clone();
                        let path = project.path.clone();
                        move |window: &mut Window, cx: &mut App| {
                            this.update(cx, |this, cx| this.open_file(path.clone(), window, cx))
                                .log_err();
                        }
                    };
                    menu.separator()
                        .entry("Manage NuGet Packages…", None, manage)
                        .entry("Open Project File", None, open)
                })
            }
            NodeKey::Package(project_path, package_id) => {
                let project_path = project_path.clone();
                let package_id = package_id.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.entry("Manage Package…", None, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.manage_packages(
                                project_path.clone(),
                                Some(package_id.clone()),
                                window,
                                cx,
                            )
                        })
                        .log_err();
                    })
                })
            }
            _ => return,
        };

        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn render_row(&self, index: usize, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let key = row.key.clone();
        let toggle_key = row.key.clone();
        let menu_key = row.key.clone();
        let is_selected = self.selected.as_ref() == Some(&row.key);
        ListItem::new(("solution-explorer-row", index))
            .indent_level(row.depth)
            .indent_step_size(px(12.))
            .toggle_state(is_selected)
            .toggle(row.expanded)
            .on_toggle(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle(&toggle_key, cx);
            }))
            .start_slot(
                Icon::new(row.icon)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(row.label))
                    .children(row.detail.map(|detail| {
                        Label::new(detail)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    })),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.activate(&key, event.click_count(), window, cx);
            }))
            .on_secondary_mouse_down(cx.listener(
                move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.deploy_context_menu(menu_key.clone(), event.position, window, cx);
                },
            ))
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.projects.read(cx);
        let startup = model
            .active_run_configuration()
            .map(RunConfiguration::label)
            .unwrap_or_else(|| "No startup project".into());
        let can_debug = model
            .active_run_configuration()
            .is_some_and(RunConfiguration::can_debug);
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(startup)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("solution-explorer-run", IconName::PlayOutlined)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title("Run", &dotnet::Run))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(dotnet::Run.boxed_clone(), cx)
                    }),
            )
            .child(
                IconButton::new("solution-explorer-debug", IconName::Debug)
                    .icon_size(IconSize::Small)
                    .disabled(!can_debug)
                    .tooltip(Tooltip::for_action_title("Debug", &dotnet::Debug))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(dotnet::Debug.boxed_clone(), cx)
                    }),
            )
            .child(
                IconButton::new("solution-explorer-build", IconName::ToolHammer)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title(
                        "Build Solution",
                        &dotnet::BuildSolution,
                    ))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(dotnet::BuildSolution.boxed_clone(), cx)
                    }),
            )
            .child(
                IconButton::new("solution-explorer-open-solution", IconName::FolderOpen)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title("Open Solution…", &OpenSolution))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(OpenSolution.boxed_clone(), cx)
                    }),
            )
    }
}

/// Asks for a solution file and opens its folder the way `workspace: open` opens a folder, then
/// focuses the Solution Explorer there.
fn open_solution(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let paths = workspace.prompt_for_open_path(
        PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open Solution".into()),
        },
        DirectoryLister::Local(workspace.project().clone(), workspace.app_state().fs.clone()),
        window,
        cx,
    );
    let multi_workspace = window.window_handle().downcast::<MultiWorkspace>();
    cx.spawn_in(window, async move |workspace, cx| {
        let Some(solution_path) = paths
            .await
            .log_err()
            .flatten()
            .and_then(|paths| paths.into_iter().next())
        else {
            return;
        };
        if !dotnet::is_solution_file(&solution_path) {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error(
                        anyhow!("{} is not a .sln or .slnx file", solution_path.display()),
                        cx,
                    );
                })
                .log_err();
            return;
        }
        let Some(directory) = solution_path.parent().map(Path::to_path_buf) else {
            return;
        };

        let opened = match multi_workspace {
            Some(handle) => handle
                .update(cx, |multi_workspace, window, cx| {
                    multi_workspace.open_project(vec![directory], OpenMode::Activate, window, cx)
                })
                .log_err(),
            None => workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_workspace_for_paths(
                        OpenMode::NewWindow,
                        vec![directory],
                        window,
                        cx,
                    )
                })
                .log_err(),
        };
        let Some(opened) = opened else {
            return;
        };
        let Some(opened) = opened.await.log_err() else {
            return;
        };
        // A folder opened in another window is focused by its own window; only a workspace in
        // this window can be focused from here.
        if multi_workspace.is_none() {
            return;
        }

        // Panels load asynchronously after the workspace opens.
        for _ in 0..PANEL_WAIT_ATTEMPTS {
            let focused = cx
                .update(|window, cx| {
                    opened.update(cx, |workspace, cx| {
                        let has_panel = workspace.panel::<SolutionExplorer>(cx).is_some();
                        if has_panel {
                            workspace.focus_panel::<SolutionExplorer>(window, cx);
                        }
                        has_panel
                    })
                })
                .unwrap_or(true);
            if focused {
                return;
            }
            cx.background_executor().timer(PANEL_WAIT_INTERVAL).await;
        }
    })
    .detach();
}

/// Adds a solution's folder to the workspace, replacing a worktree that holds only the solution
/// file (what opening a .sln file on its own creates).
fn add_solution_folder(
    workspace: &mut Workspace,
    solution_path: &Path,
    directory: &Path,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let solution_file_worktree = project
        .read(cx)
        .worktrees(cx)
        .find(|worktree| {
            let worktree = worktree.read(cx);
            worktree.is_single_file() && dotnet::same_path(&worktree.abs_path(), solution_path)
        })
        .map(|worktree| worktree.read(cx).id());
    let create_worktree =
        project.update(cx, |project, cx| project.find_or_create_worktree(directory, true, cx));
    cx.spawn(async move |_, cx| {
        create_worktree.await?;
        if let Some(worktree_id) = solution_file_worktree {
            project.update(cx, |project, cx| project.remove_worktree(worktree_id, cx));
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn solution_file_name(solution: &Solution) -> String {
    solution
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| solution.name.clone())
}

fn project_count(count: usize) -> String {
    if count == 1 {
        "1 project".into()
    } else {
        format!("{count} projects")
    }
}

fn project_icon(project: &MsBuildProject) -> IconName {
    if project.is_test_project {
        IconName::ListTodo
    } else if project.output_type == OutputType::Library {
        IconName::Book
    } else {
        IconName::FileCode
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Render for SolutionExplorer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let content = if rows.is_empty() {
            v_flex()
                .p_4()
                .gap_2()
                .child(Label::new("No .NET solutions or projects found."))
                .child(
                    Label::new("Open a solution, or a folder that contains a .sln, .slnx or project file.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Button::new("solution-explorer-open-solution", "Open Solution…").on_click(
                        |_, window, cx| window.dispatch_action(OpenSolution.boxed_clone(), cx),
                    ),
                )
                .into_any_element()
        } else {
            v_flex()
                .id("solution-explorer-rows")
                .flex_1()
                .overflow_y_scroll()
                .py_1()
                .children(
                    rows.into_iter()
                        .enumerate()
                        .map(|(index, row)| self.render_row(index, row, cx)),
                )
                .into_any_element()
        };

        v_flex()
            .key_context("SolutionExplorer")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_toolbar(cx))
            .child(content)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.context_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Focusable for SolutionExplorer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SolutionExplorer {}

impl Panel for SolutionExplorer {
    fn persistent_name() -> &'static str {
        "Solution Explorer"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        let model = self.projects.read(cx);
        (!model.projects().is_empty() || !model.solutions().is_empty()).then_some(IconName::Blocks)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Solution Explorer")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}
