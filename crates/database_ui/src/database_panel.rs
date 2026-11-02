use std::collections::{HashMap, HashSet};

use anyhow::Result;
use database::{
    ColumnSummary, ConnectionProfile, DatabaseObject, EnvironmentKind, ObjectKind, PoolKey,
};
use editor::Editor;
use futures::FutureExt as _;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClickEvent, ClipboardItem, Context, DismissEvent,
    Entity, EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent, Pixels, Point,
    Render, SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
    anchored, deferred, px, uniform_list,
};
use ui::{ContextMenu, IconButton, IconSize, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    ConnectionModal, DatabaseStore, DatabaseStoreEvent, METADATA_SESSION, NewConnection,
    OpenConnectionsFile, RefreshConnections, ToggleFocus,
    console::{self, ConsoleTarget},
};

const DATABASE_PANEL_KEY: &str = "DatabasePanel";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NodeKey {
    Environment(String),
    Connection {
        connection_id: String,
    },
    Database {
        connection_id: String,
        database: String,
    },
    Schema {
        connection_id: String,
        database: String,
        schema: String,
    },
    Object {
        connection_id: String,
        database: String,
        schema: String,
        name: String,
        kind: ObjectKind,
    },
    Column {
        connection_id: String,
        database: String,
        schema: String,
        table: String,
        column: String,
    },
}

impl NodeKey {
    fn connection_id(&self) -> Option<&str> {
        match self {
            NodeKey::Environment(_) => None,
            NodeKey::Connection { connection_id }
            | NodeKey::Database { connection_id, .. }
            | NodeKey::Schema { connection_id, .. }
            | NodeKey::Object { connection_id, .. }
            | NodeKey::Column { connection_id, .. } => Some(connection_id),
        }
    }

    fn database(&self) -> Option<&str> {
        match self {
            NodeKey::Environment(_) | NodeKey::Connection { .. } => None,
            NodeKey::Database { database, .. }
            | NodeKey::Schema { database, .. }
            | NodeKey::Object { database, .. }
            | NodeKey::Column { database, .. } => Some(database),
        }
    }
}

#[derive(Clone)]
struct ChildNode {
    key: NodeKey,
    label: SharedString,
    detail: Option<SharedString>,
    is_leaf: bool,
}

enum Children {
    Loading(#[allow(dead_code)] Task<()>),
    Loaded(Vec<ChildNode>),
    Failed(SharedString),
}

#[derive(Clone)]
enum EntryKind {
    Node {
        key: NodeKey,
        label: SharedString,
        detail: Option<SharedString>,
        is_leaf: bool,
    },
    Loading,
    Error(SharedString),
}

#[derive(Clone)]
struct Entry {
    depth: usize,
    kind: EntryKind,
}

struct DeployedContextMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    _subscription: Subscription,
}

pub struct DatabasePanel {
    workspace: WeakEntity<Workspace>,
    store: Entity<DatabaseStore>,
    focus_handle: FocusHandle,
    expanded: HashSet<NodeKey>,
    children: HashMap<NodeKey, Children>,
    entries: Vec<Entry>,
    selected: Option<NodeKey>,
    context_menu: Option<DeployedContextMenu>,
    scroll_handle: UniformListScrollHandle,
    position: DockPosition,
    _subscriptions: Vec<Subscription>,
}

impl DatabasePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let weak_workspace = workspace.weak_handle();
            cx.new(|cx| Self::new(weak_workspace, window, cx))
        })
    }

    fn new(workspace: WeakEntity<Workspace>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = DatabaseStore::global(cx);
        let subscriptions = vec![
            cx.subscribe(&store, |this, _, event, cx| match event {
                DatabaseStoreEvent::ConnectionsChanged => {
                    this.children.clear();
                    this.rebuild_entries(cx);
                }
            }),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            workspace,
            store,
            focus_handle: cx.focus_handle(),
            expanded: HashSet::default(),
            children: HashMap::default(),
            entries: Vec::new(),
            selected: None,
            context_menu: None,
            scroll_handle: UniformListScrollHandle::new(),
            position: DockPosition::Left,
            _subscriptions: subscriptions,
        };
        // Environments start expanded so connections are visible right away.
        for environment in this.store.read(cx).connections().environments() {
            this.expanded.insert(NodeKey::Environment(environment));
        }
        this.rebuild_entries(cx);
        this
    }

    fn rebuild_entries(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let connections = store.connections();
        let mut entries = Vec::new();
        for environment in connections.environments() {
            let key = NodeKey::Environment(environment.clone());
            let expanded = self.expanded.contains(&key);
            entries.push(Entry {
                depth: 0,
                kind: EntryKind::Node {
                    key,
                    label: environment.clone().into(),
                    detail: None,
                    is_leaf: false,
                },
            });
            if !expanded {
                continue;
            }
            let mut profiles = connections
                .connections
                .iter()
                .filter(|profile| profile.environment.eq_ignore_ascii_case(&environment))
                .collect::<Vec<_>>();
            profiles.sort_by_key(|profile| profile.name.to_lowercase());
            for profile in profiles {
                let key = NodeKey::Connection {
                    connection_id: profile.id.clone(),
                };
                let expanded = self.expanded.contains(&key);
                entries.push(Entry {
                    depth: 1,
                    kind: EntryKind::Node {
                        key: key.clone(),
                        label: display_name(profile).into(),
                        detail: Some(profile.driver.display_name().into()),
                        is_leaf: false,
                    },
                });
                if expanded {
                    if profile.databases.is_empty() {
                        self.push_children(&key, 2, &mut entries);
                    } else {
                        for database in &profile.databases {
                            self.push_node(
                                ChildNode {
                                    key: NodeKey::Database {
                                        connection_id: profile.id.clone(),
                                        database: database.clone(),
                                    },
                                    label: database.clone().into(),
                                    detail: None,
                                    is_leaf: false,
                                },
                                2,
                                &mut entries,
                            );
                        }
                    }
                }
            }
        }
        self.entries = entries;
        cx.notify();
    }

    fn push_node(&self, node: ChildNode, depth: usize, entries: &mut Vec<Entry>) {
        let expanded = !node.is_leaf && self.expanded.contains(&node.key);
        entries.push(Entry {
            depth,
            kind: EntryKind::Node {
                key: node.key.clone(),
                label: node.label,
                detail: node.detail,
                is_leaf: node.is_leaf,
            },
        });
        if expanded {
            self.push_children(&node.key, depth + 1, entries);
        }
    }

    fn push_children(&self, key: &NodeKey, depth: usize, entries: &mut Vec<Entry>) {
        match self.children.get(key) {
            None | Some(Children::Loading(_)) => entries.push(Entry {
                depth,
                kind: EntryKind::Loading,
            }),
            Some(Children::Failed(error)) => entries.push(Entry {
                depth,
                kind: EntryKind::Error(error.clone()),
            }),
            Some(Children::Loaded(nodes)) => {
                for node in nodes {
                    self.push_node(node.clone(), depth, entries);
                }
            }
        }
    }

    fn toggle_expanded(&mut self, key: &NodeKey, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.clone());
            if !matches!(
                self.children.get(key),
                Some(Children::Loaded(_) | Children::Loading(_))
            ) {
                self.load_children(key.clone(), cx);
            }
        }
        self.rebuild_entries(cx);
    }

    fn refresh_node(&mut self, key: &NodeKey, cx: &mut Context<Self>) {
        let mut stale = vec![key.clone()];
        while let Some(stale_key) = stale.pop() {
            if let Some(Children::Loaded(nodes)) = self.children.remove(&stale_key) {
                stale.extend(nodes.into_iter().map(|node| node.key));
            }
        }
        if self.expanded.contains(key) {
            self.load_children(key.clone(), cx);
        }
        self.rebuild_entries(cx);
    }

    fn load_children(&mut self, key: NodeKey, cx: &mut Context<Self>) {
        let (Some(connection_id), database) = (key.connection_id(), key.database()) else {
            return;
        };
        let pool_key = PoolKey {
            connection_id: connection_id.to_string(),
            database: database.unwrap_or_default().to_string(),
            session: METADATA_SESSION.to_string(),
        };
        let connection_id = connection_id.to_string();
        let task = match key.clone() {
            NodeKey::Environment(_) | NodeKey::Column { .. } => return,
            NodeKey::Connection { .. } => self.store.update(cx, |store, cx| {
                store.with_client(
                    pool_key,
                    move |client| {
                        async move {
                            let databases = client.list_databases().await?;
                            Ok(databases
                                .into_iter()
                                .map(|database| ChildNode {
                                    key: NodeKey::Database {
                                        connection_id: connection_id.clone(),
                                        database: database.clone(),
                                    },
                                    label: database.into(),
                                    detail: None,
                                    is_leaf: false,
                                })
                                .collect::<Vec<_>>())
                        }
                        .boxed()
                    },
                    cx,
                )
            }),
            NodeKey::Database { database, .. } => self.store.update(cx, |store, cx| {
                store.with_client(
                    pool_key,
                    move |client| {
                        async move {
                            let schemas = client.list_schemas().await?;
                            Ok(schemas
                                .into_iter()
                                .map(|schema| ChildNode {
                                    key: NodeKey::Schema {
                                        connection_id: connection_id.clone(),
                                        database: database.clone(),
                                        schema: schema.clone(),
                                    },
                                    label: schema.into(),
                                    detail: None,
                                    is_leaf: false,
                                })
                                .collect::<Vec<_>>())
                        }
                        .boxed()
                    },
                    cx,
                )
            }),
            NodeKey::Schema {
                database, schema, ..
            } => self.store.update(cx, |store, cx| {
                store.with_client(
                    pool_key,
                    move |client| {
                        async move {
                            let mut objects = client.list_objects(&schema).await?;
                            objects.sort_by_key(|object| {
                                (object.kind == ObjectKind::View, object.name.to_lowercase())
                            });
                            Ok(objects
                                .into_iter()
                                .map(|DatabaseObject { name, kind }| ChildNode {
                                    key: NodeKey::Object {
                                        connection_id: connection_id.clone(),
                                        database: database.clone(),
                                        schema: schema.clone(),
                                        name: name.clone(),
                                        kind,
                                    },
                                    label: name.into(),
                                    detail: (kind == ObjectKind::View).then(|| "view".into()),
                                    is_leaf: false,
                                })
                                .collect::<Vec<_>>())
                        }
                        .boxed()
                    },
                    cx,
                )
            }),
            NodeKey::Object {
                database,
                schema,
                name,
                ..
            } => self.store.update(cx, |store, cx| {
                store.with_client(
                    pool_key,
                    move |client| {
                        async move {
                            let columns = client.list_columns(&schema, &name).await?;
                            Ok(columns
                                .into_iter()
                                .map(
                                    |ColumnSummary {
                                         name: column,
                                         type_name,
                                         nullable,
                                         primary_key,
                                     }| {
                                        let mut detail = type_name;
                                        if !nullable {
                                            detail.push_str(" not null");
                                        }
                                        if primary_key {
                                            detail.push_str(" · PK");
                                        }
                                        ChildNode {
                                            key: NodeKey::Column {
                                                connection_id: connection_id.clone(),
                                                database: database.clone(),
                                                schema: schema.clone(),
                                                table: name.clone(),
                                                column: column.clone(),
                                            },
                                            label: column.into(),
                                            detail: Some(detail.into()),
                                            is_leaf: true,
                                        }
                                    },
                                )
                                .collect::<Vec<_>>())
                        }
                        .boxed()
                    },
                    cx,
                )
            }),
        };

        let loading = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    let children = match result {
                        Ok(nodes) => {
                            if let NodeKey::Connection { connection_id } = &key {
                                let databases = nodes
                                    .iter()
                                    .filter_map(|node| node.key.database().map(ToOwned::to_owned))
                                    .collect();
                                this.store.update(cx, |store, _| {
                                    store.set_known_databases(connection_id.clone(), databases)
                                });
                            }
                            Children::Loaded(nodes)
                        }
                        Err(error) => Children::Failed(format!("{error:#}").into()),
                    };
                    this.children.insert(key, children);
                    this.rebuild_entries(cx);
                })
                .log_err();
            }
        });
        self.children.insert(key, Children::Loading(loading));
    }

    fn deploy_context_menu(
        &mut self,
        key: NodeKey,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(key.clone());
        let this = cx.entity().downgrade();
        let workspace = self.workspace.clone();
        let is_connected = key
            .connection_id()
            .is_some_and(|id| self.store.read(cx).is_connected(id));
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
            let refresh = {
                let this = this.clone();
                let key = key.clone();
                move |_: &mut Window, cx: &mut App| {
                    this.update(cx, |this, cx| this.refresh_node(&key, cx))
                        .log_err();
                }
            };
            let console_entries = |menu: ContextMenu| match console_target(&key) {
                Some(target) if !matches!(key, NodeKey::Object { .. } | NodeKey::Column { .. }) => {
                    let open_workspace = workspace.clone();
                    let open_target = target.clone();
                    let bind_workspace = workspace.clone();
                    menu.entry("New Query Console", None, move |window, cx| {
                        open_workspace
                            .update(cx, |_, cx| {
                                console::open_console(open_target.clone(), None, window, cx);
                            })
                            .log_err();
                    })
                    .entry("Use for Current Editor", None, move |_, cx| {
                        bind_active_editor(&bind_workspace, target.clone(), cx);
                    })
                    .separator()
                }
                _ => menu,
            };
            match &key {
                NodeKey::Environment(_) => {
                    menu.action("New Connection…", NewConnection.boxed_clone())
                }
                NodeKey::Connection { connection_id } => {
                    let edit_id = connection_id.clone();
                    let disconnect_id = connection_id.clone();
                    let delete_id = connection_id.clone();
                    let edit_workspace = workspace.clone();
                    let delete_workspace = workspace.clone();
                    console_entries(menu)
                        .entry("Edit Connection…", None, move |window, cx| {
                            let profile = DatabaseStore::global(cx)
                                .read(cx)
                                .profile(&edit_id)
                                .cloned();
                            edit_workspace
                                .update(cx, |workspace, cx| {
                                    ConnectionModal::toggle(workspace, profile, window, cx);
                                })
                                .log_err();
                        })
                        .entry("Refresh", None, refresh)
                        .when(is_connected, |menu| {
                            menu.entry("Disconnect", None, move |_, cx| {
                                DatabaseStore::global(cx)
                                    .update(cx, |store, cx| store.disconnect(&disconnect_id, cx));
                            })
                        })
                        .separator()
                        .entry("Delete Connection", None, move |window, cx| {
                            confirm_delete(delete_workspace.clone(), delete_id.clone(), window, cx);
                        })
                }
                NodeKey::Database { .. } | NodeKey::Schema { .. } => {
                    console_entries(menu).entry("Refresh", None, refresh)
                }
                NodeKey::Object { name, .. } | NodeKey::Column { column: name, .. } => {
                    let name = name.clone();
                    let select_workspace = workspace.clone();
                    let select_key = key.clone();
                    menu.when(matches!(key, NodeKey::Object { .. }), |menu| {
                        menu.entry("Select Top 100 Rows", None, move |window, cx| {
                            open_select_top(&select_workspace, &select_key, window, cx);
                        })
                        .separator()
                    })
                    .entry("Copy Name", None, move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                    })
                    .when(matches!(key, NodeKey::Object { .. }), |menu| {
                        menu.entry("Refresh", None, refresh)
                    })
                }
            }
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some(DeployedContextMenu {
            menu,
            position,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn render_entry(&self, index: usize, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let indent = entry.depth;
        match &entry.kind {
            EntryKind::Loading => ListItem::new(("database-loading", index))
                .indent_level(indent)
                .indent_step_size(px(12.))
                .child(
                    Label::new("Loading…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            EntryKind::Error(error) => ListItem::new(("database-error", index))
                .indent_level(indent)
                .indent_step_size(px(12.))
                .tooltip(Tooltip::text(error.clone()))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                        .child(
                            Label::new(error.clone())
                                .size(LabelSize::Small)
                                .color(Color::Error)
                                .truncate(),
                        ),
                )
                .into_any_element(),
            EntryKind::Node {
                key,
                label,
                detail,
                is_leaf,
            } => {
                let store = self.store.read(cx);
                let (icon, icon_color, label_color) = match key {
                    NodeKey::Environment(name) => {
                        let color = environment_color(EnvironmentKind::from_name(name));
                        (IconName::Folder, color, color)
                    }
                    NodeKey::Connection { connection_id } => {
                        let profile = store.profile(connection_id);
                        let is_production = profile.is_some_and(ConnectionProfile::is_production);
                        let connected = store.is_connected(connection_id);
                        (
                            IconName::Server,
                            if connected {
                                Color::Success
                            } else {
                                Color::Muted
                            },
                            if is_production {
                                Color::Error
                            } else {
                                Color::Default
                            },
                        )
                    }
                    NodeKey::Database { .. } => {
                        (IconName::DatabaseZap, Color::Muted, Color::Default)
                    }
                    NodeKey::Schema { .. } => (IconName::Folder, Color::Muted, Color::Default),
                    NodeKey::Object { kind, .. } => (
                        match kind {
                            ObjectKind::Table => IconName::Table,
                            ObjectKind::View => IconName::Eye,
                        },
                        Color::Muted,
                        Color::Default,
                    ),
                    NodeKey::Column { .. } => (IconName::Hash, Color::Muted, Color::Default),
                };
                let is_expanded = self.expanded.contains(key);
                let is_selected = self.selected.as_ref() == Some(key);
                let label = if matches!(key, NodeKey::Environment(_)) {
                    SharedString::from(label.to_uppercase())
                } else {
                    label.clone()
                };
                let click_key = key.clone();
                let menu_key = key.clone();
                let is_leaf = *is_leaf;
                ListItem::new(("database-node", index))
                    .indent_level(indent)
                    .indent_step_size(px(12.))
                    .toggle((!is_leaf).then_some(is_expanded))
                    .toggle_state(is_selected)
                    .on_toggle(cx.listener({
                        let key = key.clone();
                        move |this, _, _, cx| this.toggle_expanded(&key, cx)
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.selected = Some(click_key.clone());
                        if matches!(click_key, NodeKey::Object { .. }) {
                            // Tables open their rows on double-click; the disclosure arrow lists columns.
                            if event.click_count() >= 2 {
                                open_select_top(&this.workspace, &click_key, window, cx);
                            }
                        } else if !is_leaf {
                            this.toggle_expanded(&click_key, cx);
                        }
                        cx.notify();
                    }))
                    .on_secondary_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.deploy_context_menu(menu_key.clone(), event.position, window, cx);
                        },
                    ))
                    .start_slot(Icon::new(icon).size(IconSize::Small).color(icon_color))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .child(Label::new(label).color(label_color).truncate())
                            .when_some(detail.clone(), |this, detail| {
                                this.child(
                                    Label::new(detail)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            }),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .h(px(32.))
            .px_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Databases")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("database-new-connection", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Connection"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(NewConnection.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        IconButton::new("database-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Reload Connections"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(RefreshConnections.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        IconButton::new("database-open-file", IconName::Settings)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open Connections File"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(OpenConnectionsFile.boxed_clone(), cx)
                            }),
                    ),
            )
    }
}

fn display_name(profile: &ConnectionProfile) -> String {
    if profile.name.trim().is_empty() {
        profile.host.clone()
    } else {
        profile.name.clone()
    }
}

pub(crate) fn environment_color(kind: EnvironmentKind) -> Color {
    match kind {
        EnvironmentKind::Development => Color::Success,
        EnvironmentKind::Testing => Color::Warning,
        EnvironmentKind::Production => Color::Error,
        EnvironmentKind::Other => Color::Muted,
    }
}

fn confirm_delete(
    workspace: WeakEntity<Workspace>,
    connection_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let store = DatabaseStore::global(cx);
    let Some(profile) = store.read(cx).profile(&connection_id).cloned() else {
        return;
    };
    let answer = window.prompt(
        gpui::PromptLevel::Warning,
        &format!("Delete connection \"{}\"?", display_name(&profile)),
        Some("Its saved password is removed from the credential store as well."),
        &["Delete", "Cancel"],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            let task = store.update(cx, |store, cx| store.delete_profile(&connection_id, cx));
            if let Err(error) = task.await {
                workspace
                    .update(cx, |workspace, cx| {
                        crate::show_error(workspace, &error, cx);
                    })
                    .log_err();
            }
        })
        .detach();
}

pub(crate) fn open_connections_file(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let path = database::connections_file_path();
    if !path.exists() {
        if let Err(error) = database::ConnectionsFile::default().save() {
            crate::show_error(workspace, &error, cx);
            return;
        }
    }
    workspace
        .open_abs_path(
            path,
            OpenOptions {
                visible: Some(OpenVisible::None),
                ..Default::default()
            },
            window,
            cx,
        )
        .detach_and_log_err(cx);
}

impl Render for DatabasePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let load_error = self.store.read(cx).load_error().cloned();
        let is_empty = self.store.read(cx).connections().connections.is_empty();
        v_flex()
            .id("database-panel")
            .key_context("DatabasePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_toolbar(cx))
            .when_some(load_error, |this, error| {
                this.child(
                    h_flex()
                        .p_2()
                        .gap_1()
                        .child(Icon::new(IconName::Warning).color(Color::Error))
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .map(|this| {
                if is_empty {
                    this.child(
                        v_flex()
                            .p_4()
                            .gap_2()
                            .items_center()
                            .child(Label::new("No database connections yet.").color(Color::Muted))
                            .child(
                                Button::new("database-add-first", "Add Connection")
                                    .style(ButtonStyle::Filled)
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(NewConnection.boxed_clone(), cx)
                                    }),
                            ),
                    )
                } else {
                    let entries = self.entries.clone();
                    this.child(
                        uniform_list(
                            "database-tree",
                            entries.len(),
                            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .filter_map(|index| {
                                        let entry = entries.get(index)?;
                                        Some(this.render_entry(index, entry, cx))
                                    })
                                    .collect()
                            }),
                        )
                        .flex_1()
                        .py_1()
                        .track_scroll(&self.scroll_handle),
                    )
                }
            })
            .children(self.context_menu.as_ref().map(|context_menu| {
                deferred(
                    anchored()
                        .position(context_menu.position)
                        .child(context_menu.menu.clone()),
                )
                .with_priority(3)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if this.context_menu.is_none() {
                        this.deploy_context_menu(
                            NodeKey::Environment("dev".into()),
                            event.position,
                            window,
                            cx,
                        );
                    }
                }),
            )
    }
}

impl Focusable for DatabasePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DatabasePanel {}

impl Panel for DatabasePanel {
    fn persistent_name() -> &'static str {
        "DatabasePanel"
    }

    fn panel_key() -> &'static str {
        DATABASE_PANEL_KEY
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
        px(280.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::DatabaseZap)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Database Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

fn console_target(key: &NodeKey) -> Option<ConsoleTarget> {
    Some(ConsoleTarget {
        connection_id: key.connection_id()?.to_string(),
        database: key.database().unwrap_or_default().to_string(),
    })
}

fn bind_active_editor(workspace: &WeakEntity<Workspace>, target: ConsoleTarget, cx: &mut App) {
    let Some(editor) = workspace
        .upgrade()
        .and_then(|workspace| workspace.read(cx).active_item_as::<Editor>(cx))
    else {
        return;
    };
    console::bind_editor(&editor, target, cx);
}

fn open_select_top(
    workspace: &WeakEntity<Workspace>,
    key: &NodeKey,
    window: &mut Window,
    cx: &mut App,
) {
    let NodeKey::Object { schema, name, .. } = key else {
        return;
    };
    let Some(target) = console_target(key) else {
        return;
    };
    let Some(profile) = DatabaseStore::global(cx)
        .read(cx)
        .profile(&target.connection_id)
        .cloned()
    else {
        return;
    };
    let sql = profile.driver.dialect().select_top(
        &database::sql::TableRef {
            schema: Some(schema.clone()),
            name: name.clone(),
        },
        100,
    );
    workspace
        .update(cx, |_, cx| {
            console::open_console(target, Some(sql), window, cx);
        })
        .log_err();
}

#[cfg(test)]
impl DatabasePanel {
    pub(crate) fn expand_connection_for_test(
        &mut self,
        connection_id: &str,
        cx: &mut Context<Self>,
    ) {
        let key = NodeKey::Connection {
            connection_id: connection_id.to_string(),
        };
        if !self.expanded.contains(&key) {
            self.toggle_expanded(&key, cx);
        }
    }

    pub(crate) fn visible_labels(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Node { label, .. } => Some(label.to_string()),
                EntryKind::Loading => None,
                EntryKind::Error(error) => Some(format!("error: {error}")),
            })
            .collect()
    }
}
