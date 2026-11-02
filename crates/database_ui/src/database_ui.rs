//! DataGrip-style database tools: a connection tree panel, SQL consoles and an editable result
//! grid.

mod connection_modal;
mod console;
mod database_panel;
#[cfg(test)]
mod database_ui_tests;
mod push_modal;
mod results_view;
mod sql_language;

use std::{collections::HashMap, sync::Arc};

use anyhow::{Context as _, Result};
use database::{
    Client, ConnectionPool, ConnectionProfile, ConnectionsFile, PoolKey, is_connection_error,
};
use futures::future::BoxFuture;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use gpui_tokio::Tokio;
use util::ResultExt as _;

pub use connection_modal::ConnectionModal;
pub use console::{RunScript, RunStatement, SelectConnection, default_key_bindings};
pub use database_panel::DatabasePanel;

gpui::actions!(
    database,
    [
        /// Toggles focus on the database panel.
        ToggleFocus,
        /// Adds a new database connection.
        NewConnection,
        /// Opens the JSON file that stores database connection profiles.
        OpenConnectionsFile,
        /// Reloads connection profiles and clears cached database metadata.
        RefreshConnections,
    ]
);

/// Session name used for browsing metadata, so the tree never waits on a console's query.
pub(crate) const METADATA_SESSION: &str = "metadata";

pub fn init(cx: &mut App) {
    cx.observe_new(|pane: &mut workspace::Pane, window, cx| {
        let Some(window) = window else {
            return;
        };
        let toolbar_item = cx.new(console::ConsoleToolbar::new);
        pane.toolbar().update(cx, |toolbar, cx| {
            toolbar.add_item(toolbar_item, window, cx);
        });
    })
    .detach();

    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        let languages = workspace.app_state().languages.clone();
        sql_language::register(&languages);
        console::register(workspace);
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &NewConnection, window, cx| {
            ConnectionModal::toggle(workspace, None, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenConnectionsFile, window, cx| {
            database_panel::open_connections_file(workspace, window, cx);
        });
        workspace.register_action(|_, _: &RefreshConnections, _, cx| {
            DatabaseStore::global(cx).update(cx, |store, cx| store.reload(cx));
        });
    })
    .detach();
}

#[derive(Clone, Debug)]
pub enum DatabaseStoreEvent {
    ConnectionsChanged,
}

/// Connection profiles, cached passwords and live sessions, shared by every window.
pub struct DatabaseStore {
    connections: ConnectionsFile,
    load_error: Option<SharedString>,
    pool: Arc<ConnectionPool>,
    /// Passwords read from the credential store, kept so each query doesn't hit the keychain.
    passwords: HashMap<String, String>,
    /// Databases seen while browsing each connection, offered when picking a console target.
    known_databases: HashMap<String, Vec<String>>,
}

struct GlobalDatabaseStore(Entity<DatabaseStore>);

impl Global for GlobalDatabaseStore {}

impl EventEmitter<DatabaseStoreEvent> for DatabaseStore {}

impl DatabaseStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalDatabaseStore>() {
            return store.0.clone();
        }
        let store = cx.new(|cx| {
            let mut store = Self {
                connections: ConnectionsFile::default(),
                load_error: None,
                pool: Arc::default(),
                passwords: HashMap::default(),
                known_databases: HashMap::default(),
            };
            store.reload(cx);
            store
        });
        cx.set_global(GlobalDatabaseStore(store.clone()));
        store
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalDatabaseStore>()
            .map(|store| store.0.clone())
    }

    pub fn connections(&self) -> &ConnectionsFile {
        &self.connections
    }

    pub fn load_error(&self) -> Option<&SharedString> {
        self.load_error.as_ref()
    }

    pub fn profile(&self, connection_id: &str) -> Option<&ConnectionProfile> {
        self.connections
            .connections
            .iter()
            .find(|profile| profile.id == connection_id)
    }

    pub fn known_databases(&self, connection_id: &str) -> &[String] {
        self.known_databases
            .get(connection_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn set_known_databases(&mut self, connection_id: String, databases: Vec<String>) {
        self.known_databases.insert(connection_id, databases);
    }

    pub fn is_connected(&self, connection_id: &str) -> bool {
        self.pool.is_connected(connection_id)
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        match ConnectionsFile::load() {
            Ok(connections) => {
                self.connections = connections;
                self.load_error = None;
            }
            Err(error) => {
                log::error!("failed to load database connections: {error:#}");
                self.load_error = Some(format!("{error:#}").into());
            }
        }
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
    }

    /// Adds or replaces a profile. A `Some` password is written to the OS credential store.
    pub fn save_profile(
        &mut self,
        profile: ConnectionProfile,
        password: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let mut connections = self.connections.clone();
        match connections
            .connections
            .iter_mut()
            .find(|existing| existing.id == profile.id)
        {
            Some(existing) => *existing = profile.clone(),
            None => connections.connections.push(profile.clone()),
        }
        self.pool.discard_connection(&profile.id);
        self.passwords.remove(&profile.id);
        cx.spawn(async move |this, cx| {
            if let Some(password) = &password {
                database::write_password(&profile, password, cx)
                    .await
                    .context("storing the password in the credential store")?;
            }
            let saved = connections.clone();
            cx.background_spawn(async move { saved.save() }).await?;
            this.update(cx, |this, cx| {
                this.connections = connections;
                this.load_error = None;
                cx.emit(DatabaseStoreEvent::ConnectionsChanged);
                cx.notify();
            })
        })
    }

    pub fn delete_profile(
        &mut self,
        connection_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(profile) = self.profile(connection_id).cloned() else {
            return Task::ready(Ok(()));
        };
        let mut connections = self.connections.clone();
        connections
            .connections
            .retain(|existing| existing.id != connection_id);
        self.pool.discard_connection(connection_id);
        self.passwords.remove(connection_id);
        cx.spawn(async move |this, cx| {
            if profile.needs_password() {
                database::delete_password(&profile, cx).await.log_err();
            }
            let saved = connections.clone();
            cx.background_spawn(async move { saved.save() }).await?;
            this.update(cx, |this, cx| {
                this.connections = connections;
                cx.emit(DatabaseStoreEvent::ConnectionsChanged);
                cx.notify();
            })
        })
    }

    pub fn disconnect(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        self.pool.discard_connection(connection_id);
        cx.notify();
    }

    pub fn close_session(&mut self, session: &str) {
        self.pool.discard_session(session);
    }

    /// Runs `operation` on the pooled client for `key` on the Tokio runtime, connecting first
    /// if needed. Sessions that hit connection-level errors are dropped so the next call
    /// reconnects.
    pub fn with_client<R, F>(
        &self,
        key: PoolKey,
        operation: F,
        cx: &mut Context<Self>,
    ) -> Task<Result<R>>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut Client) -> BoxFuture<'a, Result<R>> + Send + 'static,
    {
        let Some(profile) = self.profile(&key.connection_id).cloned() else {
            return Task::ready(Err(anyhow::anyhow!("the connection no longer exists")));
        };
        let pool = self.pool.clone();
        let cached_password = self.passwords.get(&profile.id).cloned();
        cx.spawn(async move |this, cx| {
            let password = if profile.needs_password()
                && cached_password.is_none()
                && pool.get(&key).is_none()
            {
                let password = database::read_password(&profile, cx)
                    .await
                    .context("reading the password from the credential store")?;
                if let Some(password) = &password {
                    this.update(cx, |this, _| {
                        this.passwords.insert(profile.id.clone(), password.clone());
                    })?;
                }
                password
            } else {
                cached_password
            };

            let connection_id = profile.id.clone();
            let task = cx.update(|cx| {
                Tokio::spawn_result(cx, {
                    let pool = pool.clone();
                    let key = key.clone();
                    async move {
                        let client = pool
                            .get_or_connect(&profile, &key, password.as_deref())
                            .await?;
                        let mut client = client.lock().await;
                        let result = operation(&mut client).await;
                        if let Err(error) = &result
                            && is_connection_error(error)
                        {
                            pool.discard(&key);
                        }
                        result
                    }
                })
            });
            let result = task.await;
            if result.is_err() && pool.get(&key).is_none() {
                // A failed login may be a stale password; read it again next time.
                this.update(cx, |this, _| this.passwords.remove(&connection_id))
                    .ok();
            }
            this.update(cx, |_, cx| cx.notify()).ok();
            result
        })
    }
}

pub(crate) fn show_error(
    workspace: &mut workspace::Workspace,
    error: &anyhow::Error,
    cx: &mut Context<workspace::Workspace>,
) {
    struct DatabaseError;
    workspace.show_toast(
        workspace::Toast::new(
            workspace::notifications::NotificationId::unique::<DatabaseError>(),
            format!("{error:#}"),
        ),
        cx,
    );
}

#[cfg(test)]
impl DatabaseStore {
    pub(crate) fn set_connections_for_test(
        &mut self,
        connections: ConnectionsFile,
        cx: &mut Context<Self>,
    ) {
        self.connections = connections;
        cx.emit(DatabaseStoreEvent::ConnectionsChanged);
        cx.notify();
    }
}
