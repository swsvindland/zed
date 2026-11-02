//! SQL consoles: binding editors to a connection and database, and running their statements.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use database::{EnvironmentKind, sql};
use editor::{Editor, MultiBufferOffset, SelectionEffects, scroll::Autoscroll};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Entity, EntityId, EventEmitter, Focusable,
    Global, KeyBinding, Render, SharedString, Subscription, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{IconButton, IconSize, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    ItemHandle, OpenOptions, OpenVisible, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
};

use crate::{DatabaseStore, database_panel::environment_color, results_view::QueryResultsView};

gpui::actions!(
    database,
    [
        /// Runs the selected SQL, or the statement under the cursor, against the editor's
        /// database connection.
        RunStatement,
        /// Runs the whole SQL file against the editor's database connection.
        RunScript,
        /// Chooses the database connection that the current SQL editor runs against.
        SelectConnection,
    ]
);

/// Key bindings for SQL editors. Zed rebuilds its keymap from the bundled JSON files on every
/// reload, so these are applied from `load_default_keymap` rather than once at startup.
pub fn default_key_bindings() -> Vec<KeyBinding> {
    let context = Some("Editor && extension == sql");
    let (run_statement, run_script) = if cfg!(target_os = "macos") {
        ("cmd-enter", "cmd-shift-enter")
    } else {
        ("ctrl-enter", "ctrl-shift-enter")
    };
    vec![
        KeyBinding::new(run_statement, RunStatement, context),
        KeyBinding::new(run_script, RunScript, context),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConsoleTarget {
    pub connection_id: String,
    /// Empty for the connection's default database.
    pub database: String,
}

/// Explicit bindings made with "Select Connection", keyed by buffer.
#[derive(Default)]
struct ConsoleBindings(HashMap<EntityId, ConsoleTarget>);

impl Global for ConsoleBindings {}

pub fn consoles_dir() -> PathBuf {
    paths::data_dir().join("database_consoles")
}

const DEFAULT_DATABASE_FILE_STEM: &str = "_default";

/// Consoles are real `.sql` files so they persist, get highlighting and keep their binding
/// across restarts: `<consoles>/<connection id>/<database>.sql`.
pub fn console_path(target: &ConsoleTarget) -> PathBuf {
    let stem = if target.database.is_empty() {
        DEFAULT_DATABASE_FILE_STEM.to_string()
    } else {
        encode_file_name(&target.database)
    };
    consoles_dir()
        .join(&target.connection_id)
        .join(format!("{stem}.sql"))
}

fn target_from_path(path: &Path) -> Option<ConsoleTarget> {
    let relative = path.strip_prefix(consoles_dir()).ok()?;
    let mut components = relative.components();
    let connection_id = components.next()?.as_os_str().to_str()?.to_string();
    let file_name = components.next()?.as_os_str().to_str()?;
    if components.next().is_some() {
        return None;
    }
    let stem = file_name.strip_suffix(".sql")?;
    let database = if stem == DEFAULT_DATABASE_FILE_STEM {
        String::new()
    } else {
        decode_file_name(stem)?
    };
    Some(ConsoleTarget {
        connection_id,
        database,
    })
}

fn encode_file_name(name: &str) -> String {
    let mut encoded = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b' ') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn decode_file_name(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = encoded.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn buffer_id(editor: &Entity<Editor>, cx: &App) -> Option<EntityId> {
    Some(
        editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()?
            .entity_id(),
    )
}

fn editor_path(editor: &Entity<Editor>, cx: &App) -> Option<PathBuf> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    Some(file.as_local()?.abs_path(cx))
}

pub fn target_for_editor(editor: &Entity<Editor>, cx: &App) -> Option<ConsoleTarget> {
    if let Some(target) =
        buffer_id(editor, cx).and_then(|id| cx.try_global::<ConsoleBindings>()?.0.get(&id).cloned())
    {
        return Some(target);
    }
    target_from_path(&editor_path(editor, cx)?)
}

pub fn is_sql_editor(editor: &Entity<Editor>, cx: &App) -> bool {
    if target_for_editor(editor, cx).is_some() {
        return true;
    }
    editor_path(editor, cx).is_some_and(|path| {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
    })
}

pub fn bind_editor(editor: &Entity<Editor>, target: ConsoleTarget, cx: &mut App) {
    let Some(id) = buffer_id(editor, cx) else {
        return;
    };
    cx.default_global::<ConsoleBindings>().0.insert(id, target);
}

/// Pool session for a console, so each one keeps its own transaction and temp tables.
pub fn session_for_buffer(buffer_id: EntityId) -> String {
    format!("console:{}", buffer_id.as_u64())
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &RunStatement, window, cx| {
        run_active_editor(workspace, false, window, cx);
    });
    workspace.register_action(|workspace, _: &RunScript, window, cx| {
        run_active_editor(workspace, true, window, cx);
    });
    workspace.register_action(|workspace, _: &SelectConnection, window, cx| {
        if let Some(editor) = workspace.active_item_as::<Editor>(cx) {
            ConnectionPicker::toggle(workspace, editor, window, cx);
        }
    });
}

/// Opens (creating if needed) the console file for `target`. With `initial_sql`, the SQL is
/// appended and run.
pub fn open_console(
    target: ConsoleTarget,
    initial_sql: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let path = console_path(&target);
    let header = {
        let store = DatabaseStore::global(cx);
        let store = store.read(cx);
        let connection = store
            .profile(&target.connection_id)
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        let database = if target.database.is_empty() {
            "default database"
        } else {
            target.database.as_str()
        };
        format!("-- {connection} · {database}\n\n")
    };
    let task = cx.spawn_in(window, async move |workspace, cx| {
        if !path.exists() {
            let path = path.clone();
            cx.background_spawn(async move {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, header)
            })
            .await?;
        }
        let item = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    path,
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })?
            .await?;
        let Some(editor) = item.downcast::<Editor>() else {
            return anyhow::Ok(());
        };
        if let Some(sql) = initial_sql {
            workspace.update_in(cx, |workspace, window, cx| {
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let text = snapshot.text();
                    let end = text.len();
                    let separator = if text.trim().is_empty() || text.ends_with("\n\n") {
                        ""
                    } else if text.ends_with('\n') {
                        "\n"
                    } else {
                        "\n\n"
                    };
                    let start = end + separator.len();
                    editor.edit(
                        [(
                            MultiBufferOffset(end)..MultiBufferOffset(end),
                            format!("{separator}{sql};\n"),
                        )],
                        cx,
                    );
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::fit()),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([
                                MultiBufferOffset(start)..MultiBufferOffset(start + sql.len())
                            ])
                        },
                    );
                });
                run_editor(workspace, &editor, false, window, cx);
            })?;
        }
        anyhow::Ok(())
    });
    task.detach_and_log_err(cx);
}

fn run_active_editor(
    workspace: &mut Workspace,
    whole_script: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
        return;
    };
    run_editor(workspace, &editor, whole_script, window, cx);
}

pub fn run_editor(
    workspace: &mut Workspace,
    editor: &Entity<Editor>,
    whole_script: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(target) = target_for_editor(editor, cx) else {
        ConnectionPicker::toggle(workspace, editor.clone(), window, cx);
        return;
    };
    let Some(buffer_id) = buffer_id(editor, cx) else {
        return;
    };
    let store = DatabaseStore::global(cx);
    let Some(profile) = store.read(cx).profile(&target.connection_id).cloned() else {
        crate::show_error(
            workspace,
            &anyhow::anyhow!("This console's connection no longer exists."),
            cx,
        );
        return;
    };
    let dialect = profile.driver.dialect();
    let (text, selection) = editor.update(cx, |editor, cx| {
        let display_snapshot = editor.display_snapshot(cx);
        let selection = editor
            .selections
            .newest::<MultiBufferOffset>(&display_snapshot);
        (
            display_snapshot.buffer_snapshot().text(),
            selection.start.0..selection.end.0,
        )
    });
    let sql = if whole_script {
        text
    } else if !selection.is_empty() {
        text.get(selection).unwrap_or_default().to_string()
    } else {
        match sql::statement_at(&text, selection.start, dialect) {
            Some(range) => text[range].to_string(),
            None => return,
        }
    };
    if sql.trim().is_empty() {
        return;
    }

    let view = QueryResultsView::find_or_create(workspace, editor, buffer_id, target, window, cx);
    view.update(cx, |view, cx| view.run(sql, window, cx));
}

pub struct ConsoleToolbar {
    editor: Option<WeakEntity<Editor>>,
    _subscriptions: Vec<Subscription>,
}

impl ConsoleToolbar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = DatabaseStore::global(cx);
        Self {
            editor: None,
            _subscriptions: vec![
                cx.observe(&store, |_, _, cx| cx.notify()),
                cx.observe_global::<ConsoleBindings>(|_, cx| cx.notify()),
            ],
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for ConsoleToolbar {}

impl ToolbarItemView for ConsoleToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.editor = active_pane_item
            .and_then(|item| item.act_as::<Editor>(cx))
            .filter(|editor| is_sql_editor(editor, cx))
            .map(|editor| editor.downgrade());
        cx.notify();
        if self.editor.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for ConsoleToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(editor) = self.editor.as_ref().and_then(WeakEntity::upgrade) else {
            return div().into_any_element();
        };
        let target = target_for_editor(&editor, cx);
        let store = DatabaseStore::global(cx);
        let profile = target
            .as_ref()
            .and_then(|target| store.read(cx).profile(&target.connection_id).cloned());
        let (label, color) = match (&target, &profile) {
            (Some(target), Some(profile)) => {
                let database = if target.database.is_empty() {
                    "default"
                } else {
                    target.database.as_str()
                };
                (
                    format!("{} · {} · {database}", profile.environment, profile.name),
                    environment_color(profile.environment_kind()),
                )
            }
            (Some(_), None) => ("Missing connection".to_string(), Color::Error),
            (None, _) => ("No connection".to_string(), Color::Muted),
        };
        let picker_editor = editor.downgrade();
        let run_editor_handle = editor.downgrade();
        h_flex()
            .gap_1()
            .child(
                Button::new("database-console-connection", label)
                    .label_size(LabelSize::Small)
                    .color(color)
                    .start_icon(
                        Icon::new(IconName::DatabaseZap)
                            .size(IconSize::Small)
                            .color(color),
                    )
                    .tooltip(Tooltip::text("Choose Connection"))
                    .on_click(move |_, window, cx| {
                        if let Some(editor) = picker_editor.upgrade() {
                            window.focus(&editor.focus_handle(cx), cx);
                            window.dispatch_action(SelectConnection.boxed_clone(), cx);
                        }
                    }),
            )
            .child(
                IconButton::new("database-console-run", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Success)
                    .tooltip(Tooltip::for_action_title(
                        "Run Statement (selection or statement under cursor)",
                        &RunStatement,
                    ))
                    .on_click(move |_, window, cx| {
                        if let Some(editor) = run_editor_handle.upgrade() {
                            window.focus(&editor.focus_handle(cx), cx);
                            window.dispatch_action(RunStatement.boxed_clone(), cx);
                        }
                    }),
            )
            .into_any_element()
    }
}

#[derive(Clone)]
struct PickerEntry {
    target: ConsoleTarget,
    label: String,
    environment: String,
    kind: EnvironmentKind,
}

pub struct ConnectionPickerDelegate {
    editor: WeakEntity<Editor>,
    entries: Vec<PickerEntry>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

pub struct ConnectionPicker;

impl ConnectionPicker {
    pub fn toggle(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let store = DatabaseStore::global(cx);
        let store = store.read(cx);
        let mut entries = Vec::new();
        for profile in &store.connections().connections {
            let mut databases = profile.databases.clone();
            if databases.is_empty() {
                databases = store.known_databases(&profile.id).to_vec();
            }
            let mut push = |database: String| {
                let shown = if database.is_empty() {
                    "default database".to_string()
                } else {
                    database.clone()
                };
                entries.push(PickerEntry {
                    label: format!("{} · {shown}", profile.name),
                    environment: profile.environment.clone(),
                    kind: profile.environment_kind(),
                    target: ConsoleTarget {
                        connection_id: profile.id.clone(),
                        database,
                    },
                });
            };
            push(String::new());
            for database in databases {
                push(database);
            }
        }
        let delegate = ConnectionPickerDelegate {
            editor: editor.downgrade(),
            matches: entries
                .iter()
                .enumerate()
                .map(|(index, entry)| StringMatch {
                    candidate_id: index,
                    score: 0.,
                    positions: Vec::new(),
                    string: entry.label.clone(),
                })
                .collect(),
            entries,
            selected_index: 0,
        };
        workspace.toggle_modal(window, cx, |window, cx| {
            Picker::uniform_list(delegate, window, cx)
        });
    }
}

impl PickerDelegate for ConnectionPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "database connection picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Run this file against…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No connections. Add one in the Database panel.".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                StringMatchCandidate::new(index, &format!("{} {}", entry.environment, entry.label))
            })
            .collect::<Vec<_>>();
        let labels = self
            .entries
            .iter()
            .map(|entry| entry.label.clone())
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                labels
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| StringMatch {
                        candidate_id: index,
                        score: 0.,
                        positions: Vec::new(),
                        string: label,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
            };
            picker
                .update(cx, |picker, _| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected_index)
            .and_then(|found| self.entries.get(found.candidate_id))
        else {
            return;
        };
        if let Some(editor) = self.editor.upgrade() {
            bind_editor(&editor, entry.target.clone(), cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(index)?;
        let entry = self.entries.get(found.candidate_id)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Label::new(entry.environment.to_uppercase())
                        .size(LabelSize::Small)
                        .color(environment_color(entry.kind)),
                )
                .child(Label::new(entry.label.clone())),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_paths_round_trip() {
        for database in ["", "Orders", "weird/name: \"x\"", "naïve"] {
            let target = ConsoleTarget {
                connection_id: "abc-123".into(),
                database: database.into(),
            };
            assert_eq!(target_from_path(&console_path(&target)), Some(target));
        }
    }
}
