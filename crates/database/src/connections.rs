use std::path::PathBuf;

use anyhow::{Context as _, Result};
use gpui::AsyncApp;
use serde::{Deserialize, Serialize};

use crate::sql::Dialect;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    SqlServer,
    Postgres,
}

impl DriverKind {
    pub fn dialect(self) -> Dialect {
        match self {
            DriverKind::SqlServer => Dialect::SqlServer,
            DriverKind::Postgres => Dialect::Postgres,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            DriverKind::SqlServer => "SQL Server",
            DriverKind::Postgres => "PostgreSQL",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            DriverKind::SqlServer => 1433,
            DriverKind::Postgres => 5432,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authentication {
    /// Windows integrated authentication (SQL Server only).
    Integrated,
    /// User name and password. The password lives in the OS credential store, never in the
    /// connections file.
    Password,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentKind {
    Development,
    Testing,
    Production,
    Other,
}

impl EnvironmentKind {
    /// Applies an explicit production flag on top of what the environment name implies.
    pub fn resolve(name: &str, production: Option<bool>) -> Self {
        match (production, Self::from_name(name)) {
            (Some(true), _) => EnvironmentKind::Production,
            (Some(false), EnvironmentKind::Production) => EnvironmentKind::Other,
            (_, kind) => kind,
        }
    }

    pub fn from_name(name: &str) -> Self {
        let name = name.trim().to_ascii_lowercase();
        if name.starts_with("prod") || name == "live" {
            EnvironmentKind::Production
        } else if ["qa", "test", "testing", "uat", "stage", "staging"].contains(&name.as_str()) {
            EnvironmentKind::Testing
        } else if ["dev", "development", "local"].contains(&name.as_str()) {
            EnvironmentKind::Development
        } else {
            EnvironmentKind::Other
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    /// Free-form environment such as `dev`, `qa` or `prod`. Connections are grouped by it.
    pub environment: String,
    pub driver: DriverKind,
    /// Host name. For SQL Server this also accepts `host\instance`, `host,port`,
    /// `(localdb)\instance` and `np:\\.\pipe\…` named pipe paths.
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub authentication: Authentication,
    #[serde(default)]
    pub username: Option<String>,
    /// Databases shown under this connection. Empty shows every database on the server.
    #[serde(default)]
    pub databases: Vec<String>,
    /// Database used for server-level queries such as listing databases.
    #[serde(default)]
    pub default_database: Option<String>,
    #[serde(default = "default_true")]
    pub encrypt: bool,
    #[serde(default)]
    pub trust_server_certificate: bool,
    /// Overrides production detection from the environment name.
    #[serde(default)]
    pub production: Option<bool>,
}

fn default_true() -> bool {
    true
}

impl ConnectionProfile {
    pub fn new(driver: DriverKind) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            environment: "dev".into(),
            driver,
            host: "localhost".into(),
            port: None,
            authentication: match driver {
                DriverKind::SqlServer => Authentication::Integrated,
                DriverKind::Postgres => Authentication::Password,
            },
            username: None,
            databases: Vec::new(),
            default_database: None,
            encrypt: true,
            trust_server_certificate: false,
            production: None,
        }
    }

    pub fn environment_kind(&self) -> EnvironmentKind {
        EnvironmentKind::resolve(&self.environment, self.production)
    }

    pub fn is_production(&self) -> bool {
        self.environment_kind() == EnvironmentKind::Production
    }

    pub fn credentials_url(&self) -> String {
        format!("zed-database://{}", self.id)
    }

    pub fn needs_password(&self) -> bool {
        self.authentication == Authentication::Password
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectionsFile {
    #[serde(default)]
    pub connections: Vec<ConnectionProfile>,
}

pub fn connections_file_path() -> PathBuf {
    paths::config_dir().join("database_connections.json")
}

impl ConnectionsFile {
    pub fn load() -> Result<Self> {
        let path = connections_file_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) if contents.trim().is_empty() => Ok(Self::default()),
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parsing {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = connections_file_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))
    }

    /// Environments in display order: dev, qa, prod first, then any others alphabetically.
    pub fn environments(&self) -> Vec<String> {
        let mut environments = Vec::<String>::new();
        for connection in &self.connections {
            if !environments
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&connection.environment))
            {
                environments.push(connection.environment.clone());
            }
        }
        environments.sort_by_key(|environment| {
            let rank = match EnvironmentKind::from_name(environment) {
                EnvironmentKind::Development => 0,
                EnvironmentKind::Testing => 1,
                EnvironmentKind::Production => 2,
                EnvironmentKind::Other => 3,
            };
            (rank, environment.to_lowercase())
        });
        environments
    }
}

pub async fn read_password(profile: &ConnectionProfile, cx: &AsyncApp) -> Result<Option<String>> {
    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
    let credentials = provider
        .read_credentials(&profile.credentials_url(), cx)
        .await?;
    credentials
        .map(|(_, password)| String::from_utf8(password).context("stored password is not UTF-8"))
        .transpose()
}

pub async fn write_password(
    profile: &ConnectionProfile,
    password: &str,
    cx: &AsyncApp,
) -> Result<()> {
    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
    let username = profile.username.clone().unwrap_or_default();
    provider
        .write_credentials(
            &profile.credentials_url(),
            &username,
            password.as_bytes(),
            cx,
        )
        .await
}

pub async fn delete_password(profile: &ConnectionProfile, cx: &AsyncApp) -> Result<()> {
    let provider = cx.update(|cx| zed_credentials_provider::global(cx));
    provider
        .delete_credentials(&profile.credentials_url(), cx)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_grouping_order() {
        let mut file = ConnectionsFile::default();
        for environment in ["prod", "Custom", "qa", "dev", "PROD"] {
            let mut profile = ConnectionProfile::new(DriverKind::SqlServer);
            profile.environment = environment.into();
            file.connections.push(profile);
        }
        assert_eq!(file.environments(), vec!["dev", "qa", "prod", "Custom"]);
        assert!(file.connections[0].is_production());
        assert!(!file.connections[2].is_production());
    }

    #[test]
    fn profile_round_trips_without_password() {
        let mut profile = ConnectionProfile::new(DriverKind::Postgres);
        profile.username = Some("app".into());
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("\"password\":"));
        let parsed: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, profile);
    }
}
