use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use dotnet::{DotnetProjects, MsBuildProject, ProjectStyle};
use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Render, SharedString, Subscription, Task, WeakEntity, Window, actions, px,
};
use ui::{ContextMenu, Divider, IconName, Label, LabelSize, ListItem, PopoverMenu, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::nuget::{NuGetClient, SearchResult, is_prerelease, package_command};

actions!(
    nuget,
    [
        /// Toggles focus on the NuGet package manager.
        ToggleFocus,
    ]
);

const PANEL_KEY: &str = "NuGetPanel";
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Browse,
    Installed,
}

/// Searches nuget.org and installs, updates or removes packages in the workspace's projects,
/// like Visual Studio's "Manage NuGet Packages".
pub struct NuGetPanel {
    projects: Entity<DotnetProjects>,
    client: Arc<NuGetClient>,
    focus_handle: FocusHandle,
    position: DockPosition,
    search_editor: Entity<Editor>,
    tab: Tab,
    include_prerelease: bool,
    project_path: Option<PathBuf>,
    results: Vec<SearchResult>,
    search_error: Option<SharedString>,
    searching: bool,
    search_task: Task<()>,
    selected_package: Option<String>,
    versions: Vec<String>,
    selected_version: Option<String>,
    versions_task: Task<()>,
    /// What the running `dotnet add/remove package` is doing, or how the last one went.
    operation_status: Option<(SharedString, bool)>,
    operation_running: bool,
    operation_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl NuGetPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                let projects = dotnet::workspace_projects(workspace, cx)
                    .context("the .NET project model is not registered")?;
                workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                    workspace.toggle_panel_focus::<NuGetPanel>(window, cx);
                });
                Ok(cx.new(|cx| Self::new(projects, window, cx)))
            })?
        })
    }

    fn new(projects: Entity<DotnetProjects>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search nuget.org", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &search_editor,
                window,
                |this, _, event: &EditorEvent, _, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        this.search(SEARCH_DEBOUNCE, cx);
                    }
                },
            ),
            cx.observe(&projects, |this, _, cx| {
                this.ensure_project(cx);
                cx.notify();
            }),
        ];
        let mut this = Self {
            projects,
            client: Arc::new(NuGetClient::new(cx.http_client())),
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            search_editor,
            tab: Tab::Browse,
            include_prerelease: false,
            project_path: None,
            results: Vec::new(),
            search_error: None,
            searching: false,
            search_task: Task::ready(()),
            selected_package: None,
            versions: Vec::new(),
            selected_version: None,
            versions_task: Task::ready(()),
            operation_status: None,
            operation_running: false,
            operation_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.ensure_project(cx);
        this.search(Duration::ZERO, cx);
        this
    }

    /// Shows `project_path`'s packages, selecting `package_id` if given.
    pub fn show_project(
        &mut self,
        project_path: PathBuf,
        package_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.project_path = Some(project_path);
        if let Some(package_id) = package_id {
            self.tab = Tab::Installed;
            self.search_editor
                .update(cx, |editor, cx| editor.set_text("", window, cx));
            self.select_package(package_id, cx);
        }
        cx.notify();
    }

    fn ensure_project(&mut self, cx: &App) {
        let projects = self.projects.read(cx).projects();
        let is_valid = self
            .project_path
            .as_ref()
            .is_some_and(|path| projects.iter().any(|project| project.path == *path));
        if !is_valid {
            self.project_path = projects
                .iter()
                .find(|project| project.style == ProjectStyle::Sdk)
                .or_else(|| projects.first())
                .map(|project| project.path.clone());
        }
    }

    fn project<'a>(&self, cx: &'a App) -> Option<&'a MsBuildProject> {
        let path = self.project_path.as_ref()?;
        self.projects
            .read(cx)
            .projects()
            .iter()
            .find(|project| project.path == *path)
    }

    fn installed_version(&self, package_id: &str, cx: &App) -> Option<Option<String>> {
        self.project(cx)?
            .package_references
            .iter()
            .find(|package| package.id.eq_ignore_ascii_case(package_id))
            .map(|package| package.version.clone())
    }

    fn search(&mut self, debounce: Duration, cx: &mut Context<Self>) {
        let query = self.search_editor.read(cx).text(cx);
        let client = self.client.clone();
        let prerelease = self.include_prerelease;
        self.searching = true;
        self.search_task = cx.spawn(async move |this, cx| {
            if !debounce.is_zero() {
                cx.background_executor().timer(debounce).await;
            }
            let results = client.search(query.trim(), prerelease).await;
            this.update(cx, |this, cx| {
                this.searching = false;
                match results {
                    Ok(results) => {
                        this.results = results;
                        this.search_error = None;
                    }
                    Err(error) => this.search_error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn select_package(&mut self, package_id: String, cx: &mut Context<Self>) {
        if self.selected_package.as_deref() == Some(package_id.as_str()) {
            return;
        }
        self.selected_package = Some(package_id.clone());
        self.versions.clear();
        self.selected_version = None;
        self.operation_status = None;
        let client = self.client.clone();
        self.versions_task = cx.spawn(async move |this, cx| {
            let versions = client.versions(&package_id).await;
            this.update(cx, |this, cx| {
                match versions {
                    Ok(versions) => {
                        this.selected_version = versions
                            .iter()
                            .find(|version| this.include_prerelease || !is_prerelease(version))
                            .or_else(|| versions.first())
                            .cloned();
                        this.versions = versions;
                    }
                    Err(error) => {
                        this.operation_status =
                            Some((format!("Couldn't load versions: {error:#}").into(), false))
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    /// Installs or updates to `version`, or removes the package when `version` is `None`.
    fn change_package(&mut self, version: Option<String>, cx: &mut Context<Self>) {
        let (Some(project), Some(package_id)) =
            (self.project(cx).cloned(), self.selected_package.clone())
        else {
            return;
        };
        let verb = match (&version, self.installed_version(&package_id, cx)) {
            (None, _) => "Removing",
            (Some(_), Some(_)) => "Updating",
            (Some(_), None) => "Installing",
        };
        let description = match &version {
            Some(version) => format!("{package_id} {version}"),
            None => package_id.clone(),
        };
        self.operation_running = true;
        self.operation_status = Some((
            format!("{verb} {description} in {}…", project.name).into(),
            true,
        ));
        let args = package_command(&project.path, &package_id, version.as_deref());
        let directory = project.directory().to_path_buf();
        self.operation_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let output = util::command::new_command("dotnet")
                        .args(&args)
                        .current_dir(&directory)
                        .output()
                        .await
                        .context("running dotnet")?;
                    if output.status.success() {
                        Ok(())
                    } else {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        Err(anyhow!("{}\n{}", stdout.trim(), stderr.trim()))
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.operation_running = false;
                this.operation_status = Some(match result {
                    Ok(()) => (format!("{verb} {description}: done").into(), true),
                    Err(error) => (
                        format!("{verb} {description} failed: {error:#}").into(),
                        false,
                    ),
                });
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project_label: SharedString = self
            .project(cx)
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "No project".into())
            .into();
        let projects = self
            .projects
            .read(cx)
            .projects()
            .iter()
            .map(|project| (project.name.clone(), project.path.clone()))
            .collect::<Vec<_>>();
        let this = cx.entity().downgrade();
        let tab_button =
            |id: &'static str, label: &'static str, tab: Tab, cx: &mut Context<Self>| {
                Button::new(id, label)
                    .label_size(LabelSize::Small)
                    .toggle_state(self.tab == tab)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }))
            };

        v_flex()
            .p_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new("Project")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        PopoverMenu::new("nuget-project")
                            .trigger(
                                Button::new("nuget-project-trigger", project_label)
                                    .label_size(LabelSize::Small)
                                    .end_icon(
                                        Icon::new(IconName::ChevronDown).size(IconSize::XSmall),
                                    ),
                            )
                            .menu(move |window, cx| {
                                let this = this.clone();
                                let projects = projects.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for (name, path) in projects {
                                        let this = this.clone();
                                        menu = menu.entry(name, None, move |window, cx| {
                                            this.update(cx, |this, cx| {
                                                this.show_project(path.clone(), None, window, cx)
                                            })
                                            .log_err();
                                        });
                                    }
                                    menu
                                }))
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(tab_button("nuget-browse", "Browse", Tab::Browse, cx))
                    .child(tab_button(
                        "nuget-installed",
                        "Installed",
                        Tab::Installed,
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("nuget-prerelease", "Prerelease")
                            .label_size(LabelSize::Small)
                            .toggle_state(self.include_prerelease)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.include_prerelease = !this.include_prerelease;
                                this.search(Duration::ZERO, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .border_1()
                    .rounded_sm()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.search_editor.clone()),
            )
    }

    fn render_packages(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search_editor.read(cx).text(cx).to_lowercase();
        let rows: Vec<(
            String,
            SharedString,
            Option<SharedString>,
            Option<SharedString>,
        )> = match self.tab {
            Tab::Browse => self
                .results
                .iter()
                .map(|result| {
                    let installed = self
                        .installed_version(&result.id, cx)
                        .map(|version| version.unwrap_or_else(|| "central".into()));
                    let detail = match installed {
                        Some(installed) if installed != result.version => {
                            format!("{installed} → {}", result.version)
                        }
                        Some(installed) => format!("{installed} installed"),
                        None => result.version.clone(),
                    };
                    (
                        result.id.clone(),
                        result.id.clone().into(),
                        Some(detail.into()),
                        (!result.description.is_empty())
                            .then(|| first_line(&result.description).into()),
                    )
                })
                .collect(),
            Tab::Installed => self
                .project(cx)
                .map(|project| {
                    project
                        .package_references
                        .iter()
                        .filter(|package| package.id.to_lowercase().contains(&query))
                        .map(|package| {
                            (
                                package.id.clone(),
                                package.id.clone().into(),
                                Some(
                                    package
                                        .version
                                        .clone()
                                        .unwrap_or_else(|| "central".into())
                                        .into(),
                                ),
                                None,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };

        let status = if self.tab == Tab::Browse && self.searching && rows.is_empty() {
            Some("Searching…".into())
        } else if self.tab == Tab::Browse
            && let Some(error) = &self.search_error
        {
            Some(error.clone())
        } else if rows.is_empty() {
            Some("No packages".into())
        } else {
            None
        };

        v_flex()
            .id("nuget-packages")
            .flex_1()
            .overflow_y_scroll()
            .py_1()
            .children(status.map(|status: SharedString| {
                div().p_2().child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            }))
            .children(rows.into_iter().enumerate().map(
                |(index, (package_id, label, detail, description))| {
                    let is_selected = self.selected_package.as_deref() == Some(package_id.as_str());
                    ListItem::new(("nuget-package", index))
                        .toggle_state(is_selected)
                        .child(
                            v_flex()
                                .child(h_flex().gap_2().child(Label::new(label)).children(
                                    detail.map(|detail| {
                                        Label::new(detail)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                    }),
                                ))
                                .children(description.map(|description| {
                                    Label::new(description)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate()
                                })),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_package(package_id.clone(), cx)
                        }))
                },
            ))
    }

    fn render_details(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let package_id = self.selected_package.clone()?;
        let project = self.project(cx)?;
        let installed = self.installed_version(&package_id, cx);
        let uses_packages_config = project.uses_packages_config;
        let description = self
            .results
            .iter()
            .find(|result| result.id.eq_ignore_ascii_case(&package_id))
            .map(|result| {
                let mut lines = vec![result.description.clone()];
                if !result.authors.is_empty() {
                    lines.push(format!("by {}", result.authors.join(", ")));
                }
                lines.push(format!("{} downloads", result.total_downloads));
                lines.join("\n")
            });

        let versions = self
            .versions
            .iter()
            .filter(|version| {
                self.include_prerelease
                    || !is_prerelease(version)
                    || Some(version.as_str()) == self.selected_version.as_deref()
            })
            .cloned()
            .collect::<Vec<_>>();
        let selected_version = self.selected_version.clone();
        let this = cx.entity().downgrade();
        let version_label: SharedString = selected_version
            .clone()
            .unwrap_or_else(|| "Loading versions…".into())
            .into();
        let disabled = self.operation_running || uses_packages_config;
        let installed_label = match &installed {
            Some(Some(version)) => format!("Installed: {version}"),
            Some(None) => "Installed (version managed centrally)".into(),
            None => "Not installed".into(),
        };
        let can_apply = selected_version.is_some()
            && installed
                .as_ref()
                .is_none_or(|installed| installed.as_ref() != selected_version.as_ref());
        let apply_label = if installed.is_some() {
            "Update"
        } else {
            "Install"
        };

        Some(
            v_flex()
                .p_2()
                .gap_2()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(Label::new(package_id))
                .children(description.map(|description| {
                    Label::new(description)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                }))
                .child(
                    Label::new(installed_label)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new("Version")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            PopoverMenu::new("nuget-version")
                                .trigger(
                                    Button::new("nuget-version-trigger", version_label)
                                        .label_size(LabelSize::Small)
                                        .end_icon(
                                            Icon::new(IconName::ChevronDown).size(IconSize::XSmall),
                                        ),
                                )
                                .menu(move |window, cx| {
                                    let this = this.clone();
                                    let versions = versions.clone();
                                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                        for version in versions {
                                            let this = this.clone();
                                            menu =
                                                menu.entry(version.clone(), None, move |_, cx| {
                                                    this.update(cx, |this, cx| {
                                                        this.selected_version =
                                                            Some(version.clone());
                                                        cx.notify();
                                                    })
                                                    .log_err();
                                                });
                                        }
                                        menu
                                    }))
                                }),
                        ),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("nuget-apply", apply_label)
                                .style(ButtonStyle::Filled)
                                .disabled(disabled || !can_apply)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let version = this.selected_version.clone();
                                    if version.is_some() {
                                        this.change_package(version, cx);
                                    }
                                })),
                        )
                        .when(installed.is_some(), |this| {
                            this.child(
                                Button::new("nuget-remove", "Remove")
                                    .disabled(disabled)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.change_package(None, cx)),
                                    ),
                            )
                        }),
                )
                .when(uses_packages_config, |this| {
                    this.child(
                        Label::new(
                            "This project uses packages.config, which dotnet add package \
                             can't change. Use Visual Studio or nuget.exe, or migrate the \
                             project to PackageReference.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                    )
                })
                .children(self.operation_status.clone().map(|(status, succeeded)| {
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(if succeeded {
                            Color::Muted
                        } else {
                            Color::Error
                        })
                })),
        )
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

impl Render for NuGetPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("NuGetPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_toolbar(cx))
            .child(self.render_packages(cx))
            .child(Divider::horizontal())
            .children(self.render_details(cx))
    }
}

impl Focusable for NuGetPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_editor.focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for NuGetPanel {}

impl Panel for NuGetPanel {
    fn persistent_name() -> &'static str {
        "NuGet"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
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
        px(360.)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        (!self.projects.read(cx).projects().is_empty()).then_some(IconName::Box)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("NuGet Packages")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}
