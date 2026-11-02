use database::{Authentication, Client, ConnectionProfile, DriverKind, EnvironmentKind};
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Task,
    Window,
};
use gpui_tokio::Tokio;
use ui::{Checkbox, ToggleButtonGroup, ToggleButtonSimple, ToggleState, prelude::*};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{DatabaseStore, database_panel::environment_color};

enum Status {
    Idle,
    Working(&'static str),
    Succeeded(SharedString),
    Failed(SharedString),
}

/// Form for adding or editing a connection profile.
pub struct ConnectionModal {
    profile: ConnectionProfile,
    /// Whether the profile already exists, so an empty password field keeps the stored one.
    is_existing: bool,
    name: Entity<InputField>,
    environment: Entity<InputField>,
    host: Entity<InputField>,
    port: Entity<InputField>,
    username: Entity<InputField>,
    password: Entity<InputField>,
    databases: Entity<InputField>,
    default_database: Entity<InputField>,
    status: Status,
    task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl ConnectionModal {
    pub fn toggle(
        workspace: &mut Workspace,
        profile: Option<ConnectionProfile>,
        window: &mut Window,
        cx: &mut gpui::Context<Workspace>,
    ) {
        workspace.toggle_modal(window, cx, |window, cx| Self::new(profile, window, cx));
    }

    fn new(
        profile: Option<ConnectionProfile>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let is_existing = profile.is_some();
        let profile = profile.unwrap_or_else(|| ConnectionProfile::new(DriverKind::SqlServer));
        let mut tab_index = 0;
        let mut field =
            |label: &str, placeholder: &str, text: &str, window: &mut Window, cx: &mut App| {
                tab_index += 1;
                let index = tab_index;
                let field = cx.new(|cx| {
                    InputField::new(window, cx, placeholder)
                        .label(label.to_string())
                        .tab_index(index)
                        .tab_stop(true)
                });
                let editor = field.read(cx).editor().clone();
                editor.set_text(text, window, cx);
                field
            };
        let name = field("Name", "Orders database", &profile.name, window, cx);
        let environment = field(
            "Environment",
            "dev, qa or prod",
            &profile.environment,
            window,
            cx,
        );
        let host = field(
            "Host",
            "localhost, server\\instance, (localdb)\\MSSQLLocalDB",
            &profile.host,
            window,
            cx,
        );
        let port = field(
            "Port",
            "Default",
            &profile
                .port
                .map(|port| port.to_string())
                .unwrap_or_default(),
            window,
            cx,
        );
        let username = field(
            "User",
            "",
            profile.username.as_deref().unwrap_or_default(),
            window,
            cx,
        );
        let password = field(
            "Password",
            if is_existing && profile.needs_password() {
                "Unchanged"
            } else {
                ""
            },
            "",
            window,
            cx,
        );
        password.update(cx, |password, cx| password.set_masked(true, window, cx));
        let databases = field(
            "Databases",
            "Comma-separated; empty shows all",
            &profile.databases.join(", "),
            window,
            cx,
        );
        let default_database = field(
            "Default database",
            "master / postgres",
            profile.default_database.as_deref().unwrap_or_default(),
            window,
            cx,
        );
        Self {
            profile,
            is_existing,
            name,
            environment,
            host,
            port,
            username,
            password,
            databases,
            default_database,
            status: Status::Idle,
            task: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn read_form(&self, cx: &App) -> Result<(ConnectionProfile, Option<String>), String> {
        let text = |field: &Entity<InputField>| field.read(cx).text(cx).trim().to_string();
        let mut profile = self.profile.clone();
        profile.name = text(&self.name);
        profile.environment = text(&self.environment);
        if profile.environment.is_empty() {
            profile.environment = "dev".into();
        }
        profile.host = text(&self.host);
        if profile.host.is_empty() {
            return Err("Enter a host.".into());
        }
        let port = text(&self.port);
        profile.port = if port.is_empty() {
            None
        } else {
            Some(
                port.parse()
                    .map_err(|_| format!("\"{port}\" isn't a valid port."))?,
            )
        };
        let username = text(&self.username);
        profile.username = (!username.is_empty()).then_some(username);
        if profile.needs_password() && profile.username.is_none() {
            return Err("Enter a user name for password authentication.".into());
        }
        profile.databases = text(&self.databases)
            .split(',')
            .map(|database| database.trim().to_string())
            .filter(|database| !database.is_empty())
            .collect();
        let default_database = text(&self.default_database);
        profile.default_database = (!default_database.is_empty()).then_some(default_database);
        let password = self.password.read(cx).text(cx);
        let password = (profile.needs_password() && !password.is_empty()).then_some(password);
        Ok((profile, password))
    }

    fn test_connection(&mut self, cx: &mut gpui::Context<Self>) {
        let (profile, password) = match self.read_form(cx) {
            Ok(form) => form,
            Err(error) => {
                self.status = Status::Failed(error.into());
                cx.notify();
                return;
            }
        };
        self.status = Status::Working("Connecting…");
        let use_stored_password =
            password.is_none() && self.is_existing && profile.needs_password();
        self.task = Some(cx.spawn(async move |this, cx| {
            let password = if use_stored_password {
                database::read_password(&profile, cx)
                    .await
                    .log_err()
                    .flatten()
            } else {
                password
            };
            let result = cx
                .update(|cx| {
                    Tokio::spawn_result(cx, async move {
                        let mut client =
                            Client::connect(&profile, None, password.as_deref()).await?;
                        client.list_databases().await
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.status = match result {
                    Ok(databases) => Status::Succeeded(
                        format!("Connected. {} databases visible.", databases.len()).into(),
                    ),
                    Err(error) => Status::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn save(&mut self, cx: &mut gpui::Context<Self>) {
        let (profile, password) = match self.read_form(cx) {
            Ok(form) => form,
            Err(error) => {
                self.status = Status::Failed(error.into());
                cx.notify();
                return;
            }
        };
        self.status = Status::Working("Saving…");
        let task = DatabaseStore::global(cx)
            .update(cx, |store, cx| store.save_profile(profile, password, cx));
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => cx.emit(DismissEvent),
                Err(error) => {
                    this.status = Status::Failed(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .log_err();
        }));
        cx.notify();
    }

    fn set_driver(&mut self, driver: DriverKind, cx: &mut gpui::Context<Self>) {
        self.profile.driver = driver;
        if driver == DriverKind::Postgres {
            self.profile.authentication = Authentication::Password;
        }
        cx.notify();
    }

    fn checkbox(
        id: &'static str,
        label: &'static str,
        checked: bool,
        cx: &mut gpui::Context<Self>,
        on_change: impl Fn(&mut Self, bool) + 'static,
    ) -> impl IntoElement {
        let this = cx.entity().downgrade();
        Checkbox::new(id, ToggleState::from(checked))
            .label(label)
            .on_click(move |state, _, cx| {
                let checked = *state == ToggleState::Selected;
                this.update(cx, |this, cx| {
                    on_change(this, checked);
                    cx.notify();
                })
                .log_err();
            })
    }
}

impl EventEmitter<DismissEvent> for ConnectionModal {}

impl Focusable for ConnectionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl ModalView for ConnectionModal {}

impl Render for ConnectionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let driver = self.profile.driver;
        let authentication = self.profile.authentication;
        let environment = self.environment.read(cx).text(cx);
        let environment_kind = EnvironmentKind::resolve(&environment, self.profile.production);
        let is_working = matches!(self.status, Status::Working(_));

        v_flex()
            .id("database-connection-modal")
            .key_context("DatabaseConnectionModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.save(cx)))
            .w(rems(36.))
            .elevation_3(cx)
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Headline::new(if self.is_existing {
                            "Edit Connection"
                        } else {
                            "New Connection"
                        })
                        .size(HeadlineSize::Small),
                    )
                    .child(
                        Label::new(match environment_kind {
                            EnvironmentKind::Production => "PRODUCTION",
                            EnvironmentKind::Testing => "TEST",
                            EnvironmentKind::Development => "DEV",
                            EnvironmentKind::Other => "",
                        })
                        .size(LabelSize::Small)
                        .color(environment_color(environment_kind)),
                    ),
            )
            .child(
                ToggleButtonGroup::single_row(
                    "database-driver",
                    [
                        ToggleButtonSimple::new(
                            "SQL Server",
                            cx.listener(|this, _, _, cx| {
                                this.set_driver(DriverKind::SqlServer, cx)
                            }),
                        ),
                        ToggleButtonSimple::new(
                            "PostgreSQL",
                            cx.listener(|this, _, _, cx| this.set_driver(DriverKind::Postgres, cx)),
                        ),
                    ],
                )
                .selected_index(match driver {
                    DriverKind::SqlServer => 0,
                    DriverKind::Postgres => 1,
                })
                .auto_width(),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(self.name.clone())
                    .child(self.environment.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(self.host.clone()))
                    .child(div().w(rems(7.)).child(self.port.clone())),
            )
            .when(driver == DriverKind::SqlServer, |this| {
                this.child(
                    ToggleButtonGroup::single_row(
                        "database-authentication",
                        [
                            ToggleButtonSimple::new(
                                "Windows authentication",
                                cx.listener(|this, _, _, cx| {
                                    this.profile.authentication = Authentication::Integrated;
                                    cx.notify();
                                }),
                            ),
                            ToggleButtonSimple::new(
                                "User and password",
                                cx.listener(|this, _, _, cx| {
                                    this.profile.authentication = Authentication::Password;
                                    cx.notify();
                                }),
                            ),
                        ],
                    )
                    .selected_index(match authentication {
                        Authentication::Integrated => 0,
                        Authentication::Password => 1,
                    })
                    .auto_width(),
                )
            })
            .when(authentication == Authentication::Password, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(self.username.clone())
                        .child(self.password.clone()),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(self.databases.clone())
                    .child(self.default_database.clone()),
            )
            .child(
                h_flex()
                    .gap_4()
                    .flex_wrap()
                    .child(Self::checkbox(
                        "database-encrypt",
                        "Encrypt",
                        self.profile.encrypt,
                        cx,
                        |this, checked| this.profile.encrypt = checked,
                    ))
                    .child(Self::checkbox(
                        "database-trust-certificate",
                        "Trust server certificate",
                        self.profile.trust_server_certificate,
                        cx,
                        |this, checked| this.profile.trust_server_certificate = checked,
                    ))
                    .child(Self::checkbox(
                        "database-production",
                        "Production (confirm before changes)",
                        environment_kind == EnvironmentKind::Production,
                        cx,
                        |this, checked| this.profile.production = Some(checked),
                    )),
            )
            .child(match &self.status {
                Status::Idle => div().into_any_element(),
                Status::Working(message) => Label::new(*message)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
                Status::Succeeded(message) => Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Success)
                    .into_any_element(),
                Status::Failed(message) => Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Button::new("database-test-connection", "Test Connection")
                            .disabled(is_working)
                            .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("database-cancel", "Cancel")
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                Button::new("database-save", "Save")
                                    .style(ButtonStyle::Filled)
                                    .disabled(is_working)
                                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                            ),
                    ),
            )
    }
}
