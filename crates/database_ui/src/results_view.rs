//! The results pane shown under a SQL console: one grid per result set plus a messages tab.

use std::time::Duration;

use database::{ConnectionProfile, Editability, PoolKey, ResultSet, StatementOutcome};
use editor::Editor;
use futures::FutureExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Task, Window,
};
use tabular_data_preview::{
    TableView,
    types::{LineNumber, TableCell, TableLikeContent},
};
use ui::{SpinnerLabel, prelude::*, table_row::TableRow};
use util::ResultExt as _;
use workspace::{Item, SplitDirection, Workspace};

use crate::{
    DatabaseStore,
    console::{ConsoleTarget, session_for_buffer},
    database_panel::environment_color,
};

pub(crate) const NULL_TEXT: &str = "NULL";

struct ResultSetView {
    result: ResultSet,
    table: Entity<TableView>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Result(usize),
    Messages,
}

enum RunState {
    Idle,
    Running(#[allow(dead_code)] Task<()>),
    Finished,
    Failed,
}

pub struct QueryResultsView {
    buffer_id: EntityId,
    target: ConsoleTarget,
    sql: String,
    state: RunState,
    elapsed: Option<Duration>,
    result_sets: Vec<ResultSetView>,
    messages: Vec<SharedString>,
    editability: Option<Editability>,
    active_tab: Tab,
    focus_handle: FocusHandle,
}

impl QueryResultsView {
    pub fn find_or_create(
        workspace: &mut Workspace,
        editor: &Entity<Editor>,
        buffer_id: EntityId,
        target: ConsoleTarget,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        for pane in workspace.panes().to_vec() {
            let existing = pane
                .read(cx)
                .items_of_type::<QueryResultsView>()
                .find(|view| view.read(cx).buffer_id == buffer_id);
            if let Some(view) = existing {
                if let Some(index) = pane.read(cx).index_for_item(&view) {
                    pane.update(cx, |pane, cx| {
                        pane.activate_item(index, false, false, window, cx)
                    });
                }
                view.update(cx, |view, _| view.target = target);
                return view;
            }
        }

        let view = cx.new(|cx| Self {
            buffer_id,
            target,
            sql: String::new(),
            state: RunState::Idle,
            elapsed: None,
            result_sets: Vec::new(),
            messages: Vec::new(),
            editability: None,
            active_tab: Tab::Messages,
            focus_handle: cx.focus_handle(),
        });
        let editor_pane = workspace
            .pane_for(editor)
            .unwrap_or_else(|| workspace.active_pane().clone());
        let results_pane = workspace.split_pane(editor_pane, SplitDirection::Down, window, cx);
        workspace.add_item(
            results_pane,
            Box::new(view.clone()),
            None,
            false,
            false,
            window,
            cx,
        );
        window.focus(&editor.focus_handle(cx), cx);
        view
    }

    fn profile(&self, cx: &App) -> Option<ConnectionProfile> {
        DatabaseStore::try_global(cx)?
            .read(cx)
            .profile(&self.target.connection_id)
            .cloned()
    }

    pub fn run(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = PoolKey {
            connection_id: self.target.connection_id.clone(),
            database: self.target.database.clone(),
            session: session_for_buffer(self.buffer_id),
        };
        self.sql = sql.clone();
        let task = DatabaseStore::global(cx).update(cx, |store, cx| {
            store.with_client(
                key,
                move |client| {
                    async move {
                        let output = client.run(&sql).await?;
                        let editability = match output.outcomes.as_slice() {
                            [StatementOutcome::Rows(result)] => Some(
                                client
                                    .editability(&sql, result)
                                    .await
                                    .unwrap_or_else(|error| Err(format!("{error:#}"))),
                            ),
                            _ => None,
                        };
                        Ok((output, editability))
                    }
                    .boxed()
                },
                cx,
            )
        });
        self.state = RunState::Running(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok((output, editability)) => this.show_output(output, editability, window, cx),
                    Err(error) => {
                        this.state = RunState::Failed;
                        this.messages = vec![format!("Error: {error:#}").into()];
                        this.active_tab = Tab::Messages;
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn show_output(
        &mut self,
        output: database::QueryOutput,
        editability: Option<Editability>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.elapsed = Some(output.elapsed);
        self.messages.clear();
        let mut result_sets = Vec::new();
        for outcome in output.outcomes {
            match outcome {
                StatementOutcome::Rows(result) => {
                    self.messages.push(
                        format!(
                            "Result {}: {} {}",
                            result_sets.len() + 1,
                            result.rows.len(),
                            if result.rows.len() == 1 {
                                "row"
                            } else {
                                "rows"
                            }
                        )
                        .into(),
                    );
                    let content = table_content(&result);
                    let table = cx.new(|cx| {
                        let mut table = TableView::new(window, cx);
                        table.set_contents(content, cx);
                        table
                    });
                    cx.observe(&table, |_, _, cx| cx.notify()).detach();
                    result_sets.push(ResultSetView { result, table });
                }
                StatementOutcome::RowsAffected(count) => self.messages.push(
                    format!(
                        "{count} {} affected",
                        if count == 1 { "row" } else { "rows" }
                    )
                    .into(),
                ),
                StatementOutcome::Completed => {
                    self.messages.push("Statement completed".into());
                }
            }
        }
        self.messages
            .push(format!("Finished in {}", format_duration(output.elapsed)).into());
        self.result_sets = result_sets;
        self.editability = editability;
        self.active_tab = if self.result_sets.is_empty() {
            Tab::Messages
        } else {
            Tab::Result(0)
        };
        self.state = RunState::Finished;
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let profile = self.profile(cx);
        let (environment, color, connection) = match &profile {
            Some(profile) => (
                profile.environment.to_uppercase(),
                environment_color(profile.environment_kind()),
                profile.name.clone(),
            ),
            None => (
                String::new(),
                Color::Muted,
                "Missing connection".to_string(),
            ),
        };
        let database = if self.target.database.is_empty() {
            "default database".to_string()
        } else {
            self.target.database.clone()
        };
        let status: AnyElement = match &self.state {
            RunState::Idle => div().into_any_element(),
            RunState::Running(_) => h_flex()
                .gap_1()
                .child(SpinnerLabel::new())
                .child(
                    Label::new("Running…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            RunState::Failed => Label::new("Failed")
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            RunState::Finished => {
                let summary = match self.active_tab {
                    Tab::Result(index) => self
                        .result_sets
                        .get(index)
                        .map(|set| {
                            let rows = set.result.rows.len();
                            format!("{rows} {}", if rows == 1 { "row" } else { "rows" })
                        })
                        .unwrap_or_default(),
                    Tab::Messages => String::new(),
                };
                let elapsed = self.elapsed.map(format_duration).unwrap_or_default();
                Label::new(
                    [summary, elapsed]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · "),
                )
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
            }
        };

        let mut tabs = h_flex().gap_0p5();
        for index in 0..self.result_sets.len() {
            let rows = self.result_sets[index].result.rows.len();
            tabs = tabs.child(
                Button::new(
                    ("database-result-tab", index),
                    format!("Result {} ({rows})", index + 1),
                )
                .label_size(LabelSize::Small)
                .toggle_state(self.active_tab == Tab::Result(index))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active_tab = Tab::Result(index);
                    cx.notify();
                })),
            );
        }
        tabs = tabs.child(
            Button::new("database-messages-tab", "Messages")
                .label_size(LabelSize::Small)
                .toggle_state(self.active_tab == Tab::Messages)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.active_tab = Tab::Messages;
                    cx.notify();
                })),
        );

        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .when(
                profile
                    .as_ref()
                    .is_some_and(ConnectionProfile::is_production),
                |this| this.bg(cx.theme().status().error_background),
            )
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(Label::new(environment).size(LabelSize::Small).color(color))
                    .child(
                        Label::new(format!("{connection} · {database}"))
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(status),
            )
            .child(tabs)
    }

    fn render_messages(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_error = matches!(self.state, RunState::Failed);
        v_flex()
            .id("database-messages")
            .size_full()
            .overflow_y_scroll()
            .p_2()
            .gap_1()
            .font_buffer(cx)
            .children(self.messages.iter().map(|message| {
                Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(if is_error {
                        Color::Error
                    } else {
                        Color::Default
                    })
            }))
            .when(!self.sql.is_empty(), |this| {
                this.child(
                    div()
                        .mt_2()
                        .pt_2()
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Label::new(self.sql.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
    }
}

pub(crate) fn table_content(result: &ResultSet) -> TableLikeContent {
    let columns = result.columns.len();
    let cell = |value: &Option<String>| match value {
        Some(value) => TableCell::Generated(value.clone().into()),
        None => TableCell::Generated(NULL_TEXT.into()),
    };
    TableLikeContent {
        number_of_cols: columns,
        headers: TableRow::from_vec(
            result
                .columns
                .iter()
                .map(|column| TableCell::Generated(column.name.clone().into()))
                .collect(),
            columns,
        ),
        rows: result
            .rows
            .iter()
            .map(|row| {
                let mut cells = row.iter().take(columns).map(cell).collect::<Vec<_>>();
                cells.resize_with(columns, || TableCell::Virtual);
                TableRow::from_vec(cells, columns)
            })
            .collect(),
        line_numbers: (1..=result.rows.len()).map(LineNumber::Line).collect(),
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() >= 1 {
        format!("{:.2} s", duration.as_secs_f64())
    } else {
        format!("{} ms", duration.as_millis())
    }
}

impl Render for QueryResultsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.active_tab {
            Tab::Result(index) => match self.result_sets.get(index) {
                Some(set) => set.table.clone().into_any_element(),
                None => self.render_messages(cx).into_any_element(),
            },
            Tab::Messages => self.render_messages(cx).into_any_element(),
        };
        v_flex()
            .key_context("DatabaseResults")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Focusable for QueryResultsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for QueryResultsView {}

impl Item for QueryResultsView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Table))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.profile(cx) {
            Some(profile) => format!("Results · {}", profile.name).into(),
            None => "Results".into(),
        }
    }
}
