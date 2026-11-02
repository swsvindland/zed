//! The results pane shown under a SQL console: one grid per result set plus a messages tab.
//! A result read from a single table with a primary key can be edited in place and pushed back.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::Duration,
};

use database::{
    ConnectionProfile, EditTarget, Editability, PoolKey, ResultSet, StatementOutcome,
    sql::{self, ParameterizedStatement, RowChange},
};
use editor::{Editor, EditorEvent, MultiBufferOffset, SelectionEffects};
use futures::FutureExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use tabular_data_preview::{
    CellDecoration, TableView, TableViewEvent,
    types::{AnyColumn, DataCellId, DataRow, LineNumber, TableCell, TableLikeContent},
};
use ui::{IconButton, IconSize, SpinnerLabel, Tooltip, prelude::*, table_row::TableRow};
use util::ResultExt as _;
use workspace::{Item, SplitDirection, Workspace};

use crate::{
    DatabaseStore,
    console::{ConsoleTarget, session_for_buffer},
    database_panel::environment_color,
    push_modal::PushReviewModal,
};

pub(crate) const NULL_TEXT: &str = "NULL";
const DEFAULT_TEXT: &str = "(default)";

/// Unsaved changes to an editable result set.
#[derive(Default)]
struct PendingEdits {
    /// New values for cells of rows read from the database, keyed by (row, column).
    updates: BTreeMap<(usize, usize), Option<String>>,
    /// Rows added in the grid. A cell is `None` until set, which leaves the column's default.
    inserts: Vec<Vec<Option<Option<String>>>>,
    deletes: BTreeSet<usize>,
}

impl PendingEdits {
    fn row_count(&self) -> usize {
        let updated_rows = self
            .updates
            .keys()
            .map(|(row, _)| *row)
            .filter(|row| !self.deletes.contains(row))
            .collect::<BTreeSet<_>>();
        updated_rows.len() + self.inserts.len() + self.deletes.len()
    }
}

struct ResultSetView {
    result: ResultSet,
    table: Entity<TableView>,
    edits: PendingEdits,
    _subscription: Subscription,
}

struct CellEdit {
    set_index: usize,
    cell: DataCellId,
    editor: Entity<Editor>,
    _subscription: Subscription,
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
    workspace: WeakEntity<Workspace>,
    buffer_id: EntityId,
    target: ConsoleTarget,
    sql: String,
    state: RunState,
    elapsed: Option<Duration>,
    result_sets: Vec<ResultSetView>,
    messages: Vec<SharedString>,
    /// Whether the single result set can be edited; `None` when several statements ran.
    editability: Option<Editability>,
    editing: Option<CellEdit>,
    /// Feedback about the last edit attempt, such as why a column can't be changed.
    notice: Option<SharedString>,
    push_task: Option<Task<()>>,
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

        let weak_workspace = workspace.weak_handle();
        let view = cx.new(|cx| Self {
            workspace: weak_workspace,
            buffer_id,
            target,
            sql: String::new(),
            state: RunState::Idle,
            elapsed: None,
            result_sets: Vec::new(),
            messages: Vec::new(),
            editability: None,
            editing: None,
            notice: None,
            push_task: None,
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

    fn pool_key(&self) -> PoolKey {
        PoolKey {
            connection_id: self.target.connection_id.clone(),
            database: self.target.database.clone(),
            session: session_for_buffer(self.buffer_id),
        }
    }

    pub(crate) fn pending_row_count(&self) -> usize {
        self.result_sets
            .iter()
            .map(|set| set.edits.row_count())
            .sum()
    }

    pub fn run(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_row_count() > 0 {
            self.messages
                .push("Discarded unsaved grid edits by running a new query.".into());
        }
        self.editing = None;
        self.notice = None;
        self.sql = sql.clone();
        let task = DatabaseStore::global(cx).update(cx, |store, cx| {
            store.with_client(
                self.pool_key(),
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
                    let set_index = result_sets.len();
                    self.messages.push(
                        format!(
                            "Result {}: {} {}",
                            set_index + 1,
                            result.rows.len(),
                            if result.rows.len() == 1 {
                                "row"
                            } else {
                                "rows"
                            }
                        )
                        .into(),
                    );
                    let content = table_content(&result, &PendingEdits::default());
                    let table = cx.new(|cx| {
                        let mut table = TableView::new(window, cx);
                        table.set_selection_enabled(true);
                        table.set_contents(content, cx);
                        table
                    });
                    let subscription = cx.subscribe_in(
                        &table,
                        window,
                        move |this, _, event: &TableViewEvent, window, cx| match event {
                            TableViewEvent::EditRequested { cell, initial_text } => {
                                this.start_edit(set_index, *cell, initial_text.clone(), window, cx);
                            }
                        },
                    );
                    result_sets.push(ResultSetView {
                        result,
                        table,
                        edits: PendingEdits::default(),
                        _subscription: subscription,
                    });
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
        for index in 0..self.result_sets.len() {
            self.refresh_decorations(index, cx);
        }
    }

    fn edit_target(&self) -> Result<&EditTarget, SharedString> {
        match &self.editability {
            Some(Ok(target)) if self.result_sets.len() == 1 => Ok(target),
            Some(Err(reason)) => Err(format!("Read-only: {reason}").into()),
            _ => Err("Read-only: results can only be edited when a single statement returns one result set.".into()),
        }
    }

    fn column_name(&self, set_index: usize, column: usize) -> String {
        self.result_sets
            .get(set_index)
            .and_then(|set| set.result.columns.get(column))
            .map(|column| column.name.clone())
            .unwrap_or_default()
    }

    fn start_edit(
        &mut self,
        set_index: usize,
        cell: DataCellId,
        initial_text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.is_some() {
            self.commit_edit(window, cx);
        }
        let target = match self.edit_target() {
            Ok(target) => target,
            Err(reason) => {
                self.notice = Some(reason);
                cx.notify();
                return;
            }
        };
        let Some(set) = self.result_sets.get(set_index) else {
            return;
        };
        let row = cell.row.0;
        let column = cell.col.0;
        if set.edits.deletes.contains(&row) {
            self.notice = Some("This row is marked for deletion. Revert to edit it.".into());
            cx.notify();
            return;
        }
        if !target.writable.get(column).copied().unwrap_or(false) {
            self.notice = Some(
                format!(
                    "Column {} is read-only (an identity, computed or expression column).",
                    self.column_name(set_index, column)
                )
                .into(),
            );
            cx.notify();
            return;
        }

        let current = cell_value(set, row, column).flatten().unwrap_or_default();
        let text = initial_text.unwrap_or(current);
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(text.clone(), window, cx);
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections
                    .select_ranges([MultiBufferOffset(text.len())..MultiBufferOffset(text.len())])
            });
            editor
        });
        let subscription = cx.subscribe_in(
            &editor,
            window,
            |this, _, event: &EditorEvent, window, cx| {
                if matches!(event, EditorEvent::Blurred) {
                    this.commit_edit(window, cx);
                }
            },
        );
        set.table.update(cx, |table, cx| {
            table.set_cell_editor(Some((cell, editor.clone().into())), cx)
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.notice = None;
        self.editing = Some(CellEdit {
            set_index,
            cell,
            editor,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.editing.take() else {
            return;
        };
        let value = edit.editor.read(cx).text(cx);
        self.set_cell(edit.set_index, edit.cell, Some(value), cx);
        self.finish_edit(&edit, window, cx);
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(edit) = self.editing.take() {
            self.finish_edit(&edit, window, cx);
        }
    }

    fn finish_edit(&mut self, edit: &CellEdit, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(set) = self.result_sets.get(edit.set_index) {
            let table = set.table.clone();
            table.update(cx, |table, cx| table.set_cell_editor(None, cx));
            window.focus(&table.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn set_cell(
        &mut self,
        set_index: usize,
        cell: DataCellId,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(set) = self.result_sets.get_mut(set_index) else {
            return;
        };
        let row = cell.row.0;
        let column = cell.col.0;
        let original_rows = set.result.rows.len();
        if row < original_rows {
            let original = set.result.rows[row].get(column).cloned().flatten();
            if original == value {
                set.edits.updates.remove(&(row, column));
            } else {
                set.edits.updates.insert((row, column), value);
            }
        } else if let Some(cells) = set.edits.inserts.get_mut(row - original_rows)
            && let Some(slot) = cells.get_mut(column)
        {
            *slot = Some(value);
        }
        self.refresh_decorations(set_index, cx);
    }

    fn selected_cell(&self, cx: &App) -> Option<(usize, DataCellId)> {
        let Tab::Result(set_index) = self.active_tab else {
            return None;
        };
        let set = self.result_sets.get(set_index)?;
        Some((set_index, set.table.read(cx).selection()?.focus))
    }

    fn set_selected_null(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_edit(window, cx);
        let Some((set_index, cell)) = self.selected_cell(cx) else {
            return;
        };
        let writable = self
            .edit_target()
            .ok()
            .and_then(|target| target.writable.get(cell.col.0).copied())
            .unwrap_or(false);
        if !writable {
            self.notice = Some("The selected column can't be changed.".into());
            cx.notify();
            return;
        }
        self.set_cell(set_index, cell, None, cx);
    }

    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_edit(window, cx);
        if let Err(reason) = self.edit_target() {
            self.notice = Some(reason);
            cx.notify();
            return;
        }
        let Some(set) = self.result_sets.first_mut() else {
            return;
        };
        set.edits.inserts.push(vec![None; set.result.columns.len()]);
        let new_row = set.result.rows.len() + set.edits.inserts.len() - 1;
        self.reload_table(0, cx);
        if let Some(set) = self.result_sets.first() {
            set.table.update(cx, |table, cx| {
                table.select_cell(DataCellId::new(DataRow(new_row), AnyColumn(0)), cx)
            });
            window.focus(&set.table.focus_handle(cx), cx);
        }
    }

    fn toggle_delete_selected_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_edit(window, cx);
        if let Err(reason) = self.edit_target() {
            self.notice = Some(reason);
            cx.notify();
            return;
        }
        let Some((set_index, cell)) = self.selected_cell(cx) else {
            return;
        };
        let Some(set) = self.result_sets.get_mut(set_index) else {
            return;
        };
        let row = cell.row.0;
        let original_rows = set.result.rows.len();
        if row >= original_rows {
            // An unsaved new row is simply dropped.
            if row - original_rows < set.edits.inserts.len() {
                set.edits.inserts.remove(row - original_rows);
            }
            self.reload_table(set_index, cx);
        } else {
            if !set.edits.deletes.remove(&row) {
                set.edits.deletes.insert(row);
            }
            self.refresh_decorations(set_index, cx);
        }
    }

    fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_edit(window, cx);
        self.notice = None;
        for index in 0..self.result_sets.len() {
            let had_inserts = !self.result_sets[index].edits.inserts.is_empty();
            self.result_sets[index].edits = PendingEdits::default();
            if had_inserts {
                self.reload_table(index, cx);
            } else {
                self.refresh_decorations(index, cx);
            }
        }
    }

    /// Rebuilds the grid's rows, needed when new rows are added or dropped.
    fn reload_table(&mut self, set_index: usize, cx: &mut Context<Self>) {
        let Some(set) = self.result_sets.get(set_index) else {
            return;
        };
        let content = table_content(&set.result, &set.edits);
        set.table
            .update(cx, |table, cx| table.set_contents(content, cx));
        self.refresh_decorations(set_index, cx);
    }

    fn refresh_decorations(&mut self, set_index: usize, cx: &mut Context<Self>) {
        let Some(set) = self.result_sets.get(set_index) else {
            return;
        };
        let status = cx.theme().status();
        let (modified, created, deleted) = (
            status.modified_background,
            status.created_background,
            status.deleted_background,
        );
        let mut cells = HashMap::default();
        let mut rows = HashMap::default();
        for (row_index, row) in set.result.rows.iter().enumerate() {
            for (column, value) in row.iter().enumerate() {
                if value.is_none() {
                    cells.insert(
                        DataCellId::new(DataRow(row_index), AnyColumn(column)),
                        CellDecoration {
                            text: None,
                            background: None,
                            muted: true,
                        },
                    );
                }
            }
        }
        for ((row, column), value) in &set.edits.updates {
            cells.insert(
                DataCellId::new(DataRow(*row), AnyColumn(*column)),
                CellDecoration {
                    text: Some(display_text(value.as_deref())),
                    background: Some(modified),
                    muted: value.is_none(),
                },
            );
        }
        let original_rows = set.result.rows.len();
        for (offset, values) in set.edits.inserts.iter().enumerate() {
            let row = original_rows + offset;
            rows.insert(
                DataRow(row),
                CellDecoration {
                    text: None,
                    background: Some(created),
                    muted: false,
                },
            );
            for (column, value) in values.iter().enumerate() {
                let cell = DataCellId::new(DataRow(row), AnyColumn(column));
                let decoration = match value {
                    None => CellDecoration {
                        text: Some(DEFAULT_TEXT.into()),
                        background: None,
                        muted: true,
                    },
                    Some(value) => CellDecoration {
                        text: Some(display_text(value.as_deref())),
                        background: None,
                        muted: value.is_none(),
                    },
                };
                cells.insert(cell, decoration);
            }
        }
        for row in &set.edits.deletes {
            rows.insert(
                DataRow(*row),
                CellDecoration {
                    text: None,
                    background: Some(deleted),
                    muted: true,
                },
            );
            cells.retain(|cell, decoration| cell.row.0 != *row || decoration.background.is_none());
        }
        set.table
            .update(cx, |table, cx| table.set_decorations(cells, rows, cx));
        cx.notify();
    }

    fn build_changes(&self, target: &EditTarget) -> Result<Vec<RowChange>, String> {
        let Some(set) = self.result_sets.first() else {
            return Ok(Vec::new());
        };
        let key_for = |row: usize| -> Result<Vec<(usize, Option<String>)>, String> {
            target
                .key_columns
                .iter()
                .map(|column| {
                    let value = set
                        .result
                        .rows
                        .get(row)
                        .and_then(|values| values.get(*column))
                        .cloned()
                        .ok_or_else(|| "A key value is missing from the result.".to_string())?;
                    Ok((*column, value))
                })
                .collect()
        };

        let mut by_row = BTreeMap::<usize, Vec<(usize, Option<String>)>>::new();
        for ((row, column), value) in &set.edits.updates {
            if !set.edits.deletes.contains(row) {
                by_row
                    .entry(*row)
                    .or_default()
                    .push((*column, value.clone()));
            }
        }
        let mut changes = Vec::new();
        for (row, values) in by_row {
            changes.push(RowChange::Update {
                key: key_for(row)?,
                values,
            });
        }
        for values in &set.edits.inserts {
            changes.push(RowChange::Insert {
                values: values
                    .iter()
                    .enumerate()
                    .filter_map(|(column, value)| Some((column, value.clone()?)))
                    .collect(),
            });
        }
        for row in &set.edits.deletes {
            changes.push(RowChange::Delete {
                key: key_for(*row)?,
            });
        }
        Ok(changes)
    }

    fn review_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        let target = match self.edit_target() {
            Ok(target) => target.clone(),
            Err(reason) => {
                self.notice = Some(reason);
                cx.notify();
                return;
            }
        };
        let Some(profile) = self.profile(cx) else {
            return;
        };
        let dialect = profile.driver.dialect();
        let statements = self.build_changes(&target).and_then(|changes| {
            sql::generate_statements(dialect, &target.table, &target.columns, &changes)
                .map_err(|error| format!("{error:#}"))
        });
        let statements = match statements {
            Ok(statements) if statements.is_empty() => return,
            Ok(statements) => statements,
            Err(error) => {
                self.notice = Some(error.into());
                cx.notify();
                return;
            }
        };
        let preview = sql::preview(dialect, &statements);
        let view = cx.entity().downgrade();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    PushReviewModal::new(profile, preview, statements, view, window, cx)
                });
            })
            .log_err();
    }

    pub(crate) fn apply(
        &mut self,
        statements: Vec<ParameterizedStatement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = statements.len();
        let task = DatabaseStore::global(cx).update(cx, |store, cx| {
            store.with_client(
                self.pool_key(),
                move |client| async move { client.apply(&statements).await }.boxed(),
                cx,
            )
        });
        self.notice = Some("Pushing changes…".into());
        self.push_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.push_task = None;
                match result {
                    Ok(_) => {
                        for set in &mut this.result_sets {
                            set.edits = PendingEdits::default();
                        }
                        let sql = this.sql.clone();
                        this.run(sql, window, cx);
                        this.notice = Some(
                            format!(
                                "Pushed {count} {} in one transaction.",
                                if count == 1 { "change" } else { "changes" }
                            )
                            .into(),
                        );
                    }
                    Err(error) => {
                        this.notice = Some(format!("Push failed: {error:#}").into());
                        this.messages.push(format!("Push failed: {error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
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

    fn render_edit_toolbar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let Tab::Result(_) = self.active_tab else {
            return None;
        };
        if !matches!(self.state, RunState::Finished) {
            return None;
        }
        let target = self.edit_target();
        let pending = self.pending_row_count();
        let is_editable = target.is_ok();
        let is_pushing = self.push_task.is_some();
        let notice = self
            .notice
            .clone()
            .or_else(|| target.as_ref().err().cloned());
        let notice_color = match &target {
            Err(_) if self.notice.is_none() => Color::Muted,
            _ => Color::Warning,
        };
        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_0p5()
                .gap_1()
                .justify_between()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    h_flex()
                        .gap_1()
                        .min_w_0()
                        .when(!is_editable, |this| {
                            this.child(
                                Icon::new(IconName::Lock)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                        .when_some(notice, |this, notice| {
                            this.child(
                                Label::new(notice)
                                    .size(LabelSize::Small)
                                    .color(notice_color)
                                    .truncate(),
                            )
                        }),
                )
                .when(is_editable, |this| {
                    this.child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                IconButton::new("database-add-row", IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Add Row"))
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.add_row(window, cx)),
                                    ),
                            )
                            .child(
                                IconButton::new("database-delete-row", IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Delete Selected Row (toggle)"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_delete_selected_row(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("database-set-null", "Set NULL")
                                    .label_size(LabelSize::Small)
                                    .tooltip(Tooltip::text("Set the selected cell to NULL"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.set_selected_null(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("database-revert", "Revert")
                                    .label_size(LabelSize::Small)
                                    .disabled(pending == 0)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.revert(window, cx)),
                                    ),
                            )
                            .child(
                                Button::new(
                                    "database-push",
                                    if pending == 0 {
                                        "Push".to_string()
                                    } else {
                                        format!("Push ({pending})")
                                    },
                                )
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Filled)
                                .disabled(pending == 0 || is_pushing)
                                .tooltip(Tooltip::text("Review the generated SQL and run it"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.review_push(window, cx)),
                                ),
                            ),
                    )
                }),
        )
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

fn cell_value(set: &ResultSetView, row: usize, column: usize) -> Option<Option<String>> {
    let original_rows = set.result.rows.len();
    if row < original_rows {
        if let Some(value) = set.edits.updates.get(&(row, column)) {
            return Some(value.clone());
        }
        set.result.rows.get(row)?.get(column).cloned()
    } else {
        set.edits
            .inserts
            .get(row - original_rows)?
            .get(column)?
            .clone()
    }
}

fn display_text(value: Option<&str>) -> SharedString {
    value.unwrap_or(NULL_TEXT).to_string().into()
}

fn table_content(result: &ResultSet, edits: &PendingEdits) -> TableLikeContent {
    let columns = result.columns.len();
    let cell = |value: &Option<String>| TableCell::Generated(display_text(value.as_deref()));
    let new_rows = edits.inserts.iter().map(|values| {
        values
            .iter()
            .map(|value| match value {
                None => TableCell::Generated(DEFAULT_TEXT.into()),
                Some(value) => cell(value),
            })
            .collect::<Vec<_>>()
    });
    let rows = result
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect::<Vec<_>>())
        .chain(new_rows)
        .map(|mut cells| {
            cells.truncate(columns);
            cells.resize_with(columns, || TableCell::Virtual);
            TableRow::from_vec(cells, columns)
        })
        .collect::<Vec<_>>();
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
        line_numbers: (1..=rows.len()).map(LineNumber::Line).collect(),
        rows,
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
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                if this.editing.is_some() {
                    this.commit_edit(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                if this.editing.is_some() {
                    this.cancel_edit(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(
                cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                    if this.editing.is_some() {
                        this.cancel_edit(window, cx);
                    } else {
                        cx.propagate();
                    }
                }),
            )
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .children(self.render_edit_toolbar(cx))
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
        let pending = self.pending_row_count();
        let name = self
            .profile(cx)
            .map(|profile| format!("Results · {}", profile.name))
            .unwrap_or_else(|| "Results".into());
        if pending > 0 {
            format!("{name} ({pending} unsaved)").into()
        } else {
            name.into()
        }
    }
}

#[cfg(test)]
impl QueryResultsView {
    pub(crate) fn summary_for_test(&self) -> (Vec<usize>, Result<(), String>) {
        (
            self.result_sets
                .iter()
                .map(|set| set.result.rows.len())
                .collect(),
            self.edit_target()
                .map(|_| ())
                .map_err(|reason| reason.to_string()),
        )
    }

    pub(crate) fn table_for_test(&self, set_index: usize) -> Entity<TableView> {
        self.result_sets[set_index].table.clone()
    }

    pub(crate) fn value_for_test(
        &self,
        set_index: usize,
        row: usize,
        column: usize,
    ) -> Option<Option<String>> {
        cell_value(self.result_sets.get(set_index)?, row, column)
    }

    pub(crate) fn is_production_for_test(&self, cx: &App) -> bool {
        self.profile(cx)
            .is_some_and(|profile| profile.is_production())
    }

    pub(crate) fn set_null_for_test(
        &mut self,
        cell: DataCellId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(set) = self.result_sets.first() {
            set.table
                .update(cx, |table, cx| table.select_cell(cell, cx));
        }
        self.set_selected_null(window, cx);
    }

    pub(crate) fn review_push_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review_push(window, cx);
    }
}

#[cfg(test)]
impl QueryResultsView {
    pub(crate) fn messages_for_test(&self) -> Vec<String> {
        self.messages.iter().map(ToString::to_string).collect()
    }
}

#[cfg(test)]
impl QueryResultsView {
    pub(crate) fn is_busy_for_test(&self) -> bool {
        self.push_task.is_some() || matches!(self.state, RunState::Running(_))
    }
}
