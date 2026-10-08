use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Task,
    WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{DotnetProjects, RunConfiguration, run_configuration};

pub(crate) struct RunConfigurationPicker {
    picker: Entity<Picker<RunConfigurationPickerDelegate>>,
}

impl RunConfigurationPicker {
    pub(crate) fn toggle(
        projects: Entity<DotnetProjects>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if projects.read(cx).run_configurations().is_empty() {
            workspace.show_error(
                anyhow::anyhow!("No runnable .NET projects found in this workspace"),
                cx,
            );
            return;
        }
        let workspace_handle = workspace.weak_handle();
        workspace.toggle_modal(window, cx, move |window, cx| {
            let delegate = RunConfigurationPickerDelegate::new(
                cx.entity().downgrade(),
                projects,
                workspace_handle,
                cx,
            );
            let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
            Self { picker }
        });
    }
}

impl Render for RunConfigurationPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl Focusable for RunConfigurationPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for RunConfigurationPicker {}
impl ModalView for RunConfigurationPicker {}

pub(crate) struct RunConfigurationPickerDelegate {
    picker: WeakEntity<RunConfigurationPicker>,
    projects: Entity<DotnetProjects>,
    workspace: WeakEntity<Workspace>,
    configurations: Vec<RunConfiguration>,
    project_paths: Vec<String>,
    candidates: Vec<StringMatchCandidate>,
    active_index: Option<usize>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl RunConfigurationPickerDelegate {
    fn new(
        picker: WeakEntity<RunConfigurationPicker>,
        projects: Entity<DotnetProjects>,
        workspace: WeakEntity<Workspace>,
        cx: &App,
    ) -> Self {
        let (configurations, project_paths, active_index) = {
            let projects = projects.read(cx);
            let configurations = projects.run_configurations().to_vec();
            let project_paths = configurations
                .iter()
                .map(|configuration| projects.display_path(&configuration.project_path, cx))
                .collect();
            let active_index = projects.active_run_configuration().and_then(|active| {
                configurations
                    .iter()
                    .position(|configuration| configuration == active)
            });
            (configurations, project_paths, active_index)
        };
        let candidates = configurations
            .iter()
            .enumerate()
            .map(|(index, configuration)| StringMatchCandidate::new(index, &configuration.label()))
            .collect();
        Self {
            picker,
            projects,
            workspace,
            configurations,
            project_paths,
            candidates,
            active_index,
            matches: Vec::new(),
            selected_index: active_index.unwrap_or(0),
        }
    }
}

impl PickerDelegate for RunConfigurationPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "run configuration picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Run a .NET project…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        let active_index = self.active_index;
        cx.spawn_in(window, async move |this, cx| {
            let query_is_empty = query.is_empty();
            let matches = if query_is_empty {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };

            this.update(cx, |this, cx| {
                let delegate = &mut this.delegate;
                delegate.selected_index = if query_is_empty {
                    active_index.unwrap_or(0)
                } else {
                    0
                };
                delegate.matches = matches;
                cx.notify();
            })
            .log_err();
        })
    }

    /// Confirming runs the configuration; secondary confirmation only selects it.
    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(configuration) = self
            .matches
            .get(self.selected_index)
            .and_then(|string_match| self.configurations.get(string_match.candidate_id))
            .cloned()
        else {
            return;
        };

        self.projects.update(cx, |projects, cx| {
            projects.select_run_configuration(configuration.clone(), cx)
        });
        if !secondary {
            let projects = self.projects.clone();
            self.workspace
                .update(cx, |_, cx| {
                    run_configuration(&projects, configuration, window, cx)
                })
                .log_err();
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let string_match = self.matches.get(index)?;
        let project_path = self.project_paths.get(string_match.candidate_id)?;
        let is_active = self.active_index == Some(string_match.candidate_id);

        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(HighlightedLabel::new(
                            string_match.string.clone(),
                            string_match.positions.clone(),
                        ))
                        .child(
                            Label::new(project_path.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .when(is_active, |item| {
                    item.end_slot(Icon::new(IconName::Check).color(Color::Muted))
                }),
        )
    }
}
