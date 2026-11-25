//! The results pane shown under a SQL console: one grid per result set plus a messages tab.
//! Rows arrive a page at a time (see [`database::Execution`]). A result read from a single table
//! with a primary key can be edited in place and pushed back.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context as _, anyhow};
use chrono::{DateTime, Local};
use database::{
    ColumnInfo, ConnectionProfile, EditTarget, Editability, ExecutionEnd, ExecutionEvent,
    ExecutionHandle, Fetch, PoolKey, ResultSet,
    sql::{self, ParameterizedStatement, RowChange},
};
use editor::{Editor, EditorEvent, MultiBufferOffset, SelectionEffects};
use futures::{FutureExt as _, StreamExt as _, channel::oneshot};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Global,
    Render, SharedString, Subscription, Task, WeakEntity, Window,
};
use tabular_data_preview::{
    CellDecoration, TableView, TableViewEvent,
    types::{AnyColumn, DataCellId, DataRow, LineNumber, TableCell, TableLikeContent},
};
use ui::{IconButton, IconSize, SpinnerLabel, Tooltip, prelude::*, table_row::TableRow};
use util::ResultExt as _;
use workspace::{Item, SplitDirection, Toast, Workspace, notifications::NotificationId};

use crate::{
    DatabaseStore,
    console::{ConsoleTarget, session_for_buffer},
    database_panel::environment_color,
    export::{self, ExportData, ExportFormat, RunStatus},
    push_modal::PushReviewModal,
};

gpui::actions!(
    database,
    [
        /// Stops the running statement, or closes a result that is waiting at the fetch limit.
        StopStatement,
        /// Fetches the next page of a result that stopped at the fetch limit.
        FetchNextPage,
        /// Fetches every remaining row of a result that stopped at the fetch limit.
        FetchAllRows,
        /// Saves the results to a Markdown, JSON, CSV or TSV file, picked by its extension.
        ExportResults,
    ]
);

const DEFAULT_TEXT: &str = "(default)";
/// While rows stream in, the grid is rebuilt at most this often.
const GRID_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// Consoles (by buffer) whose statement is running or holding an open result, so the console
/// toolbar can offer Stop.
#[derive(Default)]
pub(crate) struct ActiveConsoleRuns(pub HashSet<EntityId>);

impl Global for ActiveConsoleRuns {}

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
    /// Grid cells for `result.rows`, converted once as each page arrives.
    cells: Vec<TableRow<TableCell>>,
    /// Rows arrived since the grid was last rebuilt.
    grid_stale: bool,
    table: Entity<TableView>,
    edits: PendingEdits,
    _subscription: Subscription,
}

impl ResultSetView {
    fn append(&mut self, rows: Vec<Vec<Option<String>>>) {
        let columns = self.result.columns.len();
        self.cells.extend(
            rows.iter()
                .map(|row| table_row(row.iter().map(value_cell), columns)),
        );
        self.result.rows.extend(rows);
        self.grid_stale = true;
    }
}

struct CellEdit {
    set_index: usize,
    cell: DataCellId,
    editor: Entity<Editor>,
    /// Leaving a NULL (or a new row's unset) cell without typing keeps it as it was, instead of
    /// turning it into an empty string.
    keeps_value_unless_edited: bool,
    edited: bool,
    _subscription: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Result(usize),
    Messages,
}

#[derive(Clone, PartialEq, Eq)]
enum RunState {
    Idle,
    /// The server is working, or rows are streaming in.
    Running,
    /// The latest result stopped at the fetch limit and has more rows on the server.
    Paused,
    Stopping,
    Finished,
    /// Stopped early, on request or by the query timeout.
    Stopped(SharedString),
    Failed,
}

impl RunState {
    #[cfg(test)]
    fn is_active(&self) -> bool {
        matches!(
            self,
            RunState::Running | RunState::Paused | RunState::Stopping
        )
    }
}

/// A run in progress. Dropping it cancels the statement.
struct ActiveRun {
    handle: ExecutionHandle,
    _task: Task<anyhow::Result<()>>,
}

enum Outcome {
    ResultSet(usize),
    RowsAffected(u64),
}

/// An export waiting for a paused run to fetch its remaining rows.
struct PendingExport {
    path: PathBuf,
    /// The result set written by formats that hold one table.
    table: usize,
}

pub struct QueryResultsView {
    workspace: WeakEntity<Workspace>,
    buffer_id: EntityId,
    target: ConsoleTarget,
    sql: String,
    state: RunState,
    run: Option<ActiveRun>,
    fetch_limit: Option<usize>,
    /// Time the run has spent waiting at the fetch limit, which isn't query time.
    paused_for: Duration,
    paused_since: Option<Instant>,
    elapsed: Option<Duration>,
    started_at: Option<DateTime<Local>>,
    pending_export: Option<PendingExport>,
    export_task: Option<Task<()>>,
    result_sets: Vec<ResultSetView>,
    outcomes: Vec<Outcome>,
    error: Option<SharedString>,
    /// Lines shown above the outcomes, such as discarded edits.
    notes: Vec<SharedString>,
    messages: Vec<SharedString>,
    /// Whether the single result set can be edited; `None` when several statements ran.
    editability: Option<Editability>,
    editing: Option<CellEdit>,
    /// Feedback about the last edit attempt, such as why a column can't be changed.
    notice: Option<SharedString>,
    push_task: Option<Task<()>>,
    active_tab: Tab,
    last_grid_refresh: Instant,
    grid_refresh: Option<Task<()>>,
    focus_handle: FocusHandle,
    _release: Subscription,
}

impl QueryResultsView {
    pub fn find(workspace: &Workspace, buffer_id: EntityId, cx: &App) -> Option<Entity<Self>> {
        workspace.panes().iter().find_map(|pane| {
            pane.read(cx)
                .items_of_type::<QueryResultsView>()
                .find(|view| view.read(cx).buffer_id == buffer_id)
        })
    }

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
            run: None,
            fetch_limit: None,
            paused_for: Duration::ZERO,
            paused_since: None,
            elapsed: None,
            started_at: None,
            pending_export: None,
            export_task: None,
            result_sets: Vec::new(),
            outcomes: Vec::new(),
            error: None,
            notes: Vec::new(),
            messages: Vec::new(),
            editability: None,
            editing: None,
            notice: None,
            push_task: None,
            active_tab: Tab::Messages,
            last_grid_refresh: Instant::now(),
            grid_refresh: None,
            focus_handle: cx.focus_handle(),
            // Closing the pane cancels its run, so its console no longer offers Stop.
            _release: cx.on_release(|this: &mut Self, cx| this.set_console_active(false, cx)),
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
        let discarded_edits = self.pending_row_count() > 0;
        // Cancels the previous run; it finishes on its own before this one gets the session.
        self.run = None;
        self.grid_refresh = None;
        self.editing = None;
        self.notice = None;
        self.sql = sql.clone();
        self.notes = if discarded_edits {
            vec!["Discarded unsaved grid edits by running a new query.".into()]
        } else {
            Vec::new()
        };
        self.result_sets.clear();
        self.outcomes.clear();
        self.error = None;
        self.editability = None;
        self.elapsed = None;
        self.started_at = Some(Local::now());
        // An export waiting on the previous run's rows would otherwise write this run's.
        self.pending_export = None;
        self.paused_for = Duration::ZERO;
        self.paused_since = None;
        self.active_tab = Tab::Messages;
        // The first rows of a run show at once; only later updates are throttled.
        self.last_grid_refresh = Instant::now()
            .checked_sub(GRID_REFRESH_INTERVAL)
            .unwrap_or_else(Instant::now);

        let Some(profile) = self.profile(cx) else {
            self.finish_run(
                Err(anyhow!("This console's connection no longer exists.")),
                Duration::ZERO,
                cx,
            );
            return;
        };
        let options = profile.execution_options();
        self.fetch_limit = options.fetch_limit;
        // Describing first keeps the session free for the statement, which may hold it open at
        // the fetch limit.
        let describe = sql::split_statements(&sql, profile.driver.dialect()).len() == 1
            && sql::may_return_rows(&sql);
        let (handle, mut execution, mut events) = database::execution(options);
        let (editability_sender, editability_receiver) = oneshot::channel();
        let task = DatabaseStore::global(cx).update(cx, |store, cx| {
            store.with_client(
                self.pool_key(),
                move |client| {
                    async move {
                        let editability = if describe {
                            Some(
                                client
                                    .editability(&sql)
                                    .await
                                    .unwrap_or_else(|error| Err(format!("{error:#}"))),
                            )
                        } else {
                            None
                        };
                        if editability_sender.send(editability).is_err() {
                            log::debug!("the results view closed before the query ran");
                        }
                        client.execute(&sql, &mut execution).await
                    }
                    .boxed()
                },
                cx,
            )
        });
        // A run dropped mid-statement would leave its session out of sync with the server, so it
        // always runs to the end; dropping the handle along with this view cancels it instead.
        let (result_sender, result_receiver) = oneshot::channel();
        cx.background_spawn(async move {
            if result_sender.send(task.await).is_err() {
                log::debug!("the results view closed before the query finished");
            }
        })
        .detach();

        let started = Instant::now();
        let task = cx.spawn_in(window, async move |this, cx| {
            let editability = editability_receiver.await.ok().flatten();
            this.update(cx, |this, cx| {
                this.editability = editability;
                cx.notify();
            })?;
            while let Some(event) = events.next().await {
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                this.update_in(cx, |this, window, cx| this.apply_events(batch, window, cx))?;
            }
            let result = result_receiver
                .await
                .unwrap_or_else(|_| Err(anyhow!("the query was abandoned")));
            this.update(cx, |this, cx| {
                this.finish_run(result, started.elapsed(), cx)
            })
        });
        self.run = Some(ActiveRun {
            handle,
            _task: task,
        });
        self.state = RunState::Running;
        self.set_console_active(true, cx);
        self.rebuild_messages();
        cx.notify();
    }

    fn set_console_active(&self, active: bool, cx: &mut App) {
        let runs = &mut cx.default_global::<ActiveConsoleRuns>().0;
        if active {
            runs.insert(self.buffer_id);
        } else {
            runs.remove(&self.buffer_id);
        }
    }

    fn apply_events(
        &mut self,
        events: Vec<ExecutionEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for event in events {
            match event {
                ExecutionEvent::ResultSet(columns) => self.add_result_set(columns, window, cx),
                ExecutionEvent::Rows(rows) => {
                    let Some(set_index) = self.result_sets.len().checked_sub(1) else {
                        continue;
                    };
                    // New rows go before the unsaved new rows, so an edit of one would move.
                    if self.editing.as_ref().is_some_and(|edit| {
                        edit.set_index == set_index
                            && self
                                .result_sets
                                .get(set_index)
                                .is_some_and(|set| edit.cell.row.0 >= set.result.rows.len())
                    }) {
                        self.commit_edit(window, cx);
                    }
                    if let Some(set) = self.result_sets.get_mut(set_index) {
                        set.append(rows);
                    }
                }
                ExecutionEvent::RowsAffected(count) => {
                    self.outcomes.push(Outcome::RowsAffected(count));
                }
                ExecutionEvent::Paused => {
                    if self.state == RunState::Running {
                        self.state = RunState::Paused;
                        self.paused_since = Some(Instant::now());
                    }
                }
            }
        }
        let streaming = self.state == RunState::Running;
        self.refresh_stale_grids(!streaming, cx);
        self.rebuild_messages();
        cx.notify();
    }

    fn add_result_set(
        &mut self,
        columns: Vec<ColumnInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let set_index = self.result_sets.len();
        let table = cx.new(|cx| {
            let mut table = TableView::new(window, cx);
            table.set_selection_enabled(true);
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
        self.result_sets.push(ResultSetView {
            result: ResultSet {
                columns,
                rows: Vec::new(),
            },
            cells: Vec::new(),
            grid_stale: true,
            table,
            edits: PendingEdits::default(),
            _subscription: subscription,
        });
        self.outcomes.push(Outcome::ResultSet(set_index));
        if set_index == 0 {
            self.active_tab = Tab::Result(0);
        }
    }

    /// Rebuilds grids that have new rows, at most every [`GRID_REFRESH_INTERVAL`] unless
    /// `now`, since each rebuild copies every row.
    fn refresh_stale_grids(&mut self, now: bool, cx: &mut Context<Self>) {
        if !self.result_sets.iter().any(|set| set.grid_stale) {
            return;
        }
        let since_last = self.last_grid_refresh.elapsed();
        if !now && since_last < GRID_REFRESH_INTERVAL {
            if self.grid_refresh.is_none() {
                let delay = GRID_REFRESH_INTERVAL - since_last;
                self.grid_refresh = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(delay).await;
                    this.update(cx, |this, cx| {
                        this.grid_refresh = None;
                        this.refresh_stale_grids(true, cx);
                    })
                    .log_err();
                }));
            }
            return;
        }
        self.grid_refresh = None;
        self.last_grid_refresh = Instant::now();
        for index in 0..self.result_sets.len() {
            if self
                .result_sets
                .get(index)
                .is_some_and(|set| set.grid_stale)
            {
                self.reload_table(index, cx);
            }
        }
    }

    fn finish_run(
        &mut self,
        result: anyhow::Result<ExecutionEnd>,
        elapsed: Duration,
        cx: &mut Context<Self>,
    ) {
        self.run = None;
        self.set_console_active(false, cx);
        let paused = self.paused_for
            + self
                .paused_since
                .take()
                .map_or(Duration::ZERO, |since| since.elapsed());
        self.elapsed = Some(elapsed.saturating_sub(paused));
        // Even a failed script may have changed the schema before it stopped.
        if let Some(profile) = self.profile(cx)
            && database::completion::may_change_schema(&self.sql, profile.driver.dialect())
            && let Some(store) = DatabaseStore::try_global(cx)
        {
            let target = self.target.clone();
            store.update(cx, |store, _| {
                store.invalidate_catalog(&target.connection_id, Some(&target.database))
            });
        }
        match result {
            Ok(ExecutionEnd::Completed) => self.state = RunState::Finished,
            Ok(ExecutionEnd::Cancelled) => self.state = RunState::Stopped("Stopped".into()),
            Ok(ExecutionEnd::TimedOut(timeout)) => {
                self.state = RunState::Stopped(
                    format!(
                        "Cancelled by the {}-second query timeout",
                        timeout.as_secs()
                    )
                    .into(),
                )
            }
            Err(error) => {
                self.state = RunState::Failed;
                self.error = Some(format!("{error:#}").into());
                self.active_tab = Tab::Messages;
            }
        }
        self.refresh_stale_grids(true, cx);
        self.rebuild_messages();
        if let Some(pending) = self.pending_export.take() {
            match self.state {
                RunState::Finished => self.write_export(pending.path, pending.table, cx),
                RunState::Failed => self.show_export_toast(
                    "Export cancelled: the run failed before every row arrived.".into(),
                    false,
                    cx,
                ),
                _ => self.show_export_toast(
                    "Export cancelled: the run stopped before every row arrived.".into(),
                    false,
                    cx,
                ),
            }
        }
        cx.notify();
    }

    fn can_export(&self) -> bool {
        !self.result_sets.is_empty()
            && self.pending_export.is_none()
            && matches!(
                self.state,
                RunState::Finished | RunState::Paused | RunState::Stopped(_) | RunState::Failed
            )
    }

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_export() {
            return;
        }
        let directory = self
            .workspace
            .upgrade()
            .and_then(|workspace| {
                let project = workspace.read(cx).project().read(cx);
                project
                    .visible_worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            })
            .unwrap_or_else(|| paths::home_dir().clone());
        let file_name = export::suggested_file_name(
            &self.target.database,
            self.started_at.unwrap_or_else(Local::now),
        );
        let prompt = cx.prompt_for_new_path(&directory, Some(&file_name));
        cx.spawn_in(window, async move |this, cx| match prompt.await {
            Ok(Ok(Some(path))) => this
                .update_in(cx, |this, window, cx| this.export_to(path, window, cx))
                .log_err(),
            Ok(Err(error)) => this
                .update(cx, |this, cx| {
                    this.show_export_toast(format!("Export failed: {error:#}"), false, cx)
                })
                .log_err(),
            Ok(Ok(None)) | Err(_) => None,
        })
        .detach();
    }

    /// Writes the results to `path`, fetching every remaining row first when the run is paused.
    fn export_to(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let table = match self.active_tab {
            Tab::Result(index) => index,
            Tab::Messages => 0,
        };
        match self.state {
            RunState::Paused => {
                self.pending_export = Some(PendingExport { path, table });
                self.fetch(Fetch::All, window, cx);
            }
            RunState::Running => self.pending_export = Some(PendingExport { path, table }),
            RunState::Finished | RunState::Stopped(_) | RunState::Failed => {
                self.write_export(path, table, cx)
            }
            RunState::Idle | RunState::Stopping => {}
        }
        cx.notify();
    }

    fn export_data(&self, cx: &App) -> ExportData {
        let profile = self.profile(cx);
        let status = match &self.state {
            RunState::Finished => RunStatus::Completed,
            RunState::Stopped(reason) => RunStatus::Stopped(reason.to_string()),
            RunState::Failed => RunStatus::Failed(
                self.error
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            ),
            RunState::Idle | RunState::Running | RunState::Paused | RunState::Stopping => {
                RunStatus::Stopped("Still running".into())
            }
        };
        ExportData {
            connection: profile
                .as_ref()
                .map(|profile| profile.name.clone())
                .unwrap_or_default(),
            environment: profile
                .as_ref()
                .map(|profile| profile.environment.clone())
                .unwrap_or_default(),
            database: self.target.database.clone(),
            started_at: self.started_at,
            elapsed: self.elapsed,
            status,
            sql: self.sql.clone(),
            result_sets: self
                .result_sets
                .iter()
                .map(|set| set.result.clone())
                .collect(),
            messages: self.messages.iter().map(ToString::to_string).collect(),
        }
    }

    fn write_export(&mut self, path: PathBuf, table: usize, cx: &mut Context<Self>) {
        let data = self.export_data(cx);
        let format = ExportFormat::from_path(&path);
        let write = cx.background_spawn({
            let path = path.clone();
            async move {
                let text = export::render(format, &data, table);
                std::fs::write(&path, text)
                    .with_context(|| format!("writing {}", path.display()))?;
                anyhow::Ok(export::exported_row_count(format, &data, table))
            }
        });
        self.export_task = Some(cx.spawn(async move |this, cx| {
            let (message, succeeded) = match write.await {
                Ok(rows) => (
                    format!("Exported {} to {}", row_count_text(rows), path.display()),
                    true,
                ),
                Err(error) => (format!("Export failed: {error:#}"), false),
            };
            this.update(cx, |this, cx| {
                this.show_export_toast(message, succeeded, cx)
            })
            .log_err();
        }));
    }

    /// Failures stay up until dismissed, so they aren't missed.
    fn show_export_toast(&self, message: String, autohide: bool, cx: &mut App) {
        struct ExportToast;
        let toast = Toast::new(NotificationId::unique::<ExportToast>(), message);
        let toast = if autohide { toast.autohide() } else { toast };
        self.workspace
            .update(cx, |workspace, cx| workspace.show_toast(toast, cx))
            .log_err();
    }

    fn rebuild_messages(&mut self) {
        let mut messages = self.notes.clone();
        let last_set = self.result_sets.len().checked_sub(1);
        for outcome in &self.outcomes {
            match outcome {
                Outcome::ResultSet(index) => {
                    let rows = self
                        .result_sets
                        .get(*index)
                        .map_or(0, |set| set.result.rows.len());
                    let more = if self.state == RunState::Paused && last_set == Some(*index) {
                        " so far; more on the server"
                    } else {
                        ""
                    };
                    messages.push(
                        format!("Result {}: {}{more}", index + 1, row_count_text(rows)).into(),
                    );
                }
                Outcome::RowsAffected(count) => messages.push(
                    format!(
                        "{count} {} affected",
                        if *count == 1 { "row" } else { "rows" }
                    )
                    .into(),
                ),
            }
        }
        if self.outcomes.is_empty() && self.state == RunState::Finished {
            messages.push("Statement completed".into());
        }
        if let Some(error) = &self.error {
            messages.push(format!("Error: {error}").into());
        }
        if let RunState::Stopped(reason) = &self.state {
            messages.push(reason.clone());
        }
        if let Some(elapsed) = self.elapsed {
            let verb = if matches!(self.state, RunState::Stopped(_)) {
                "Ran for"
            } else {
                "Finished in"
            };
            messages.push(format!("{verb} {}", format_duration(elapsed)).into());
        }
        self.messages = messages;
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &self.run else {
            return;
        };
        run.handle.cancel();
        if matches!(self.state, RunState::Running | RunState::Paused) {
            self.state = RunState::Stopping;
        }
        cx.notify();
    }

    fn fetch(&mut self, fetch: Fetch, window: &mut Window, cx: &mut Context<Self>) {
        if self.state != RunState::Paused {
            return;
        }
        self.commit_edit(window, cx);
        if let Some(since) = self.paused_since.take() {
            self.paused_for += since.elapsed();
        }
        if let Some(run) = &self.run {
            run.handle.fetch(fetch);
            self.state = RunState::Running;
            self.rebuild_messages();
            cx.notify();
        }
    }

    /// Stops the statement or open result, if any. Returns whether there was one.
    pub(crate) fn stop_if_running(&mut self, cx: &mut Context<Self>) -> bool {
        let running = self.run.is_some();
        self.stop(cx);
        running
    }

    fn edit_target(&self) -> Result<&EditTarget, SharedString> {
        match &self.editability {
            Some(Ok(target)) if self.result_sets.len() == 1 => {
                let columns_match = self
                    .result_sets
                    .first()
                    .is_some_and(|set| set.result.columns.len() == target.columns.len());
                if columns_match {
                    Ok(target)
                } else {
                    Err("Read-only: the result's columns don't match the statement's description."
                        .into())
                }
            }
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

        let current = cell_value(set, row, column).flatten();
        let keeps_value_unless_edited = current.is_none() && initial_text.is_none();
        let text = initial_text.unwrap_or_else(|| current.unwrap_or_default());
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            if keeps_value_unless_edited {
                editor.set_placeholder_text(TableCell::NULL_TEXT, window, cx);
            }
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
            |this, _, event: &EditorEvent, window, cx| match event {
                EditorEvent::Edited { .. } => {
                    if let Some(edit) = this.editing.as_mut() {
                        edit.edited = true;
                    }
                }
                EditorEvent::Blurred => this.commit_edit(window, cx),
                _ => {}
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
            keeps_value_unless_edited,
            edited: false,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.editing.take() else {
            return;
        };
        if !(edit.keeps_value_unless_edited && !edit.edited) {
            let value = edit.editor.read(cx).text(cx);
            self.set_cell(edit.set_index, edit.cell, Some(value), cx);
        }
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
        if let Some(values) = set.result.rows.get(row) {
            let original = values.get(column).cloned().flatten();
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
            let Some(set) = self.result_sets.get_mut(index) else {
                continue;
            };
            let had_inserts = !set.edits.inserts.is_empty();
            set.edits = PendingEdits::default();
            if had_inserts {
                self.reload_table(index, cx);
            } else {
                self.refresh_decorations(index, cx);
            }
        }
    }

    /// Rebuilds the grid's rows, needed when rows arrive or new rows are added or dropped.
    fn reload_table(&mut self, set_index: usize, cx: &mut Context<Self>) {
        let Some(set) = self.result_sets.get_mut(set_index) else {
            return;
        };
        set.grid_stale = false;
        let content = table_content(set);
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
        // An open result holds the session. Close it so the push can run; the query is run
        // again afterwards anyway.
        self.stop(cx);
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
                    Ok(applied) => {
                        for set in &mut this.result_sets {
                            set.edits = PendingEdits::default();
                        }
                        let sql = this.sql.clone();
                        this.run(sql, window, cx);
                        let changes = if count == 1 { "change" } else { "changes" };
                        this.notice = Some(
                            if applied.in_open_transaction {
                                format!(
                                    "Pushed {count} {changes} into the console's open \
                                     transaction. They're saved when it commits."
                                )
                            } else {
                                format!("Pushed {count} {changes} in one transaction.")
                            }
                            .into(),
                        );
                    }
                    Err(error) => {
                        this.notice = Some(format!("Push failed: {error:#}").into());
                        this.notes.push(format!("Push failed: {error:#}").into());
                        this.rebuild_messages();
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
        let summary = match self.active_tab {
            Tab::Result(index) => self
                .result_sets
                .get(index)
                .map(|set| self.rows_label(index, set.result.rows.len()))
                .unwrap_or_default(),
            Tab::Messages => String::new(),
        };
        let muted_label = |text: String| {
            Label::new(text)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };
        let status: AnyElement = match &self.state {
            RunState::Idle => div().into_any_element(),
            RunState::Running | RunState::Stopping => h_flex()
                .gap_1()
                .child(SpinnerLabel::new())
                .child(muted_label(match (&self.state, summary.is_empty()) {
                    (RunState::Stopping, _) => "Stopping…".to_string(),
                    (_, true) => "Running…".to_string(),
                    (_, false) => format!("{summary} · fetching…"),
                }))
                .into_any_element(),
            RunState::Paused => muted_label(summary),
            RunState::Failed => Label::new("Failed")
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            RunState::Stopped(reason) => h_flex()
                .gap_1()
                .when(!summary.is_empty(), |this| this.child(muted_label(summary)))
                .child(
                    Label::new(reason.clone())
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
                .into_any_element(),
            RunState::Finished => {
                let elapsed = self.elapsed.map(format_duration).unwrap_or_default();
                muted_label(
                    [summary, elapsed]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(" · "),
                )
            }
        };

        let run_controls = h_flex()
            .gap_0p5()
            .when(self.state == RunState::Paused, |this| {
                let next_page = match self.fetch_limit {
                    Some(limit) => format!("Next {limit}"),
                    None => "Next Page".to_string(),
                };
                this.child(
                    Button::new("database-fetch-next-page", next_page)
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::for_action_title(
                            "Fetch the next page of rows",
                            &FetchNextPage,
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.fetch(Fetch::NextPage, window, cx)
                        })),
                )
                .child(
                    Button::new("database-fetch-all", "All")
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::for_action_title(
                            "Fetch every remaining row",
                            &FetchAllRows,
                        ))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.fetch(Fetch::All, window, cx)),
                        ),
                )
            })
            .when(
                matches!(self.state, RunState::Running | RunState::Paused),
                |this| {
                    this.child(
                        IconButton::new("database-stop", IconName::Stop)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Error)
                            .tooltip(Tooltip::for_action_title(
                                if self.state == RunState::Paused {
                                    "Close the Open Result"
                                } else {
                                    "Stop the Statement"
                                },
                                &StopStatement,
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                    )
                },
            );

        let mut tabs = h_flex().gap_0p5();
        for (index, set) in self.result_sets.iter().enumerate() {
            let rows = set.result.rows.len();
            let count = if self.has_more_rows(index) {
                format!("{rows}+")
            } else {
                rows.to_string()
            };
            tabs = tabs.child(
                Button::new(
                    ("database-result-tab", index),
                    format!("Result {} ({count})", index + 1),
                )
                .label_size(LabelSize::Small)
                .toggle_state(self.active_tab == Tab::Result(index))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active_tab = Tab::Result(index);
                    cx.notify();
                })),
            );
        }
        tabs = tabs
            .child(
                Button::new("database-messages-tab", "Messages")
                    .label_size(LabelSize::Small)
                    .toggle_state(self.active_tab == Tab::Messages)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.active_tab = Tab::Messages;
                        cx.notify();
                    })),
            )
            .child(
                IconButton::new("database-export", IconName::Download)
                    .icon_size(IconSize::Small)
                    .disabled(!self.can_export())
                    .tooltip(Tooltip::for_action_title(
                        if self.state == RunState::Paused {
                            "Fetch Every Row and Export…"
                        } else {
                            "Export Results…"
                        },
                        &ExportResults,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.export(window, cx))),
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
                    .child(status)
                    .child(run_controls),
            )
            .child(tabs)
    }

    /// Whether result set `index` stopped at the fetch limit with rows left on the server.
    fn has_more_rows(&self, index: usize) -> bool {
        self.state == RunState::Paused && index + 1 == self.result_sets.len()
    }

    fn rows_label(&self, index: usize, rows: usize) -> String {
        if self.has_more_rows(index) {
            format!("{} · more available", row_count_text(rows))
        } else {
            row_count_text(rows)
        }
    }

    fn render_edit_toolbar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let Tab::Result(_) = self.active_tab else {
            return None;
        };
        if !matches!(
            self.state,
            RunState::Finished | RunState::Paused | RunState::Stopped(_)
        ) {
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

pub(crate) fn row_count_text(rows: usize) -> String {
    format!("{rows} {}", if rows == 1 { "row" } else { "rows" })
}

fn display_text(value: Option<&str>) -> SharedString {
    value.unwrap_or(TableCell::NULL_TEXT).to_string().into()
}

fn value_cell(value: &Option<String>) -> TableCell {
    match value {
        Some(text) => TableCell::Generated(text.clone().into()),
        None => TableCell::Null,
    }
}

fn table_row(cells: impl Iterator<Item = TableCell>, columns: usize) -> TableRow<TableCell> {
    let mut cells = cells.collect::<Vec<_>>();
    cells.truncate(columns);
    cells.resize_with(columns, || TableCell::Virtual);
    TableRow::from_vec(cells, columns)
}

fn table_content(set: &ResultSetView) -> TableLikeContent {
    let columns = set.result.columns.len();
    let new_rows = set.edits.inserts.iter().map(|values| {
        table_row(
            values.iter().map(|value| match value {
                None => TableCell::Generated(DEFAULT_TEXT.into()),
                Some(value) => value_cell(value),
            }),
            columns,
        )
    });
    let rows = set
        .cells
        .iter()
        .cloned()
        .chain(new_rows)
        .collect::<Vec<_>>();
    TableLikeContent {
        number_of_cols: columns,
        headers: TableRow::from_vec(
            set.result
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

pub(crate) fn format_duration(duration: Duration) -> String {
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
            .on_action(cx.listener(|this, _: &StopStatement, _, cx| this.stop(cx)))
            .on_action(cx.listener(|this, _: &FetchNextPage, window, cx| {
                this.fetch(Fetch::NextPage, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &FetchAllRows, window, cx| {
                    this.fetch(Fetch::All, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ExportResults, window, cx| this.export(window, cx)))
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

    pub(crate) fn messages_for_test(&self) -> Vec<String> {
        self.messages.iter().map(ToString::to_string).collect()
    }

    pub(crate) fn is_busy_for_test(&self) -> bool {
        self.push_task.is_some() || self.state.is_active()
    }

    pub(crate) fn is_paused_for_test(&self) -> bool {
        self.state == RunState::Paused
    }

    pub(crate) fn stopped_reason_for_test(&self) -> Option<String> {
        match &self.state {
            RunState::Stopped(reason) => Some(reason.to_string()),
            _ => None,
        }
    }

    pub(crate) fn export_to_for_test(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.export_to(path, window, cx);
    }

    pub(crate) fn show_result_tab_for_test(&mut self, index: usize, cx: &mut Context<Self>) {
        self.active_tab = Tab::Result(index);
        cx.notify();
    }

    pub(crate) fn fetch_for_test(
        &mut self,
        all: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fetch(if all { Fetch::All } else { Fetch::NextPage }, window, cx);
    }

    pub(crate) fn grid_cell_for_test(
        &self,
        set_index: usize,
        row: usize,
        column: usize,
        cx: &App,
    ) -> Option<TableCell> {
        let set = self.result_sets.get(set_index)?;
        let table = set.table.read(cx);
        table
            .contents()
            .rows
            .get(row)?
            .get(AnyColumn(column))
            .cloned()
    }
}
