use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result, bail};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient};
use serde::Deserialize;

const SERVICE_INDEX: &str = "https://api.nuget.org/v3/index.json";

/// A client for the nuget.org [v3 API](https://learn.microsoft.com/nuget/api/overview).
pub struct NuGetClient {
    http: Arc<dyn HttpClient>,
    endpoints: Mutex<Option<Endpoints>>,
}

#[derive(Clone)]
struct Endpoints {
    search: String,
    package_base: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, deserialize_with = "authors")]
    pub authors: Vec<String>,
    #[serde(default)]
    pub total_downloads: u64,
    #[serde(default)]
    pub verified: bool,
}

/// `authors` is a string or a list of strings, depending on the feed.
fn authors<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Authors {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Authors::deserialize(deserializer)? {
        Authors::One(author) => vec![author],
        Authors::Many(authors) => authors,
    })
}

impl NuGetClient {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self {
            http,
            endpoints: Mutex::new(None),
        }
    }

    fn cached_endpoints(&self) -> Option<Endpoints> {
        self.endpoints.lock().ok()?.clone()
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let mut response = self.http.get(url, AsyncBody::default(), true).await?;
        let mut body = Vec::new();
        response
            .body_mut()
            .read_to_end(&mut body)
            .await
            .with_context(|| format!("reading {url}"))?;
        if !response.status().is_success() {
            bail!("{url} returned {}", response.status());
        }
        serde_json::from_slice(&body).with_context(|| format!("parsing {url}"))
    }

    async fn endpoints(&self) -> Result<Endpoints> {
        if let Some(endpoints) = self.cached_endpoints() {
            return Ok(endpoints);
        }

        #[derive(Deserialize)]
        struct ServiceIndex {
            resources: Vec<Resource>,
        }
        #[derive(Deserialize)]
        struct Resource {
            #[serde(rename = "@id")]
            id: String,
            #[serde(rename = "@type")]
            kind: String,
        }

        let index: ServiceIndex = self.get_json(SERVICE_INDEX).await?;
        let find = |prefix: &str| {
            index
                .resources
                .iter()
                .find(|resource| resource.kind.starts_with(prefix))
                .map(|resource| resource.id.trim_end_matches('/').to_owned())
                .with_context(|| format!("the NuGet service index has no {prefix}"))
        };
        let endpoints = Endpoints {
            search: find("SearchQueryService")?,
            package_base: find("PackageBaseAddress/3.0.0")?,
        };
        if let Ok(mut cached) = self.endpoints.lock() {
            *cached = Some(endpoints.clone());
        }
        Ok(endpoints)
    }

    pub async fn search(&self, query: &str, prerelease: bool) -> Result<Vec<SearchResult>> {
        #[derive(Deserialize)]
        struct SearchResponse {
            data: Vec<SearchResult>,
        }

        let endpoints = self.endpoints().await?;
        let url = format!(
            "{}?q={}&take=30&prerelease={prerelease}&semVerLevel=2.0.0",
            endpoints.search,
            percent_encode(query)
        );
        let response: SearchResponse = self.get_json(&url).await?;
        Ok(response.data)
    }

    /// All published versions of a package, newest first.
    pub async fn versions(&self, package_id: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct VersionIndex {
            versions: Vec<String>,
        }

        let endpoints = self.endpoints().await?;
        let url = format!(
            "{}/{}/index.json",
            endpoints.package_base,
            percent_encode(&package_id.to_lowercase())
        );
        let index: VersionIndex = self.get_json(&url).await?;
        let mut versions = index.versions;
        versions.reverse();
        Ok(versions)
    }
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Whether `version` is a prerelease (`1.0.0-beta.1`), ignoring build metadata.
pub fn is_prerelease(version: &str) -> bool {
    version
        .split('+')
        .next()
        .is_some_and(|version| version.contains('-'))
}

/// The `dotnet` command that installs or updates (`Some(version)`) or removes (`None`) a package
/// in an SDK-style project.
pub fn package_command(project: &Path, package_id: &str, version: Option<&str>) -> Vec<String> {
    let project = project.to_string_lossy().into_owned();
    match version {
        Some(version) => vec![
            "add".into(),
            project,
            "package".into(),
            package_id.into(),
            "--version".into(),
            version.into(),
        ],
        None => vec![
            "remove".into(),
            project,
            "package".into(),
            package_id.into(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::FakeHttpClient;
    use pretty_assertions::assert_eq;

    #[gpui::test]
    async fn searches_and_lists_versions() {
        let http = FakeHttpClient::create(|request| async move {
            let uri = request.uri().to_string();
            let body = if uri == SERVICE_INDEX {
                r#"{"version": "3.0.0", "resources": [
                    {"@id": "https://search.example/query", "@type": "SearchQueryService/3.5.0"},
                    {"@id": "https://packages.example/v3/", "@type": "PackageBaseAddress/3.0.0"}
                ]}"#
            } else if uri
                == "https://search.example/query?q=Newtonsoft%20Json&take=30&prerelease=false&semVerLevel=2.0.0"
            {
                r#"{"totalHits": 1, "data": [{"id": "Newtonsoft.Json", "version": "13.0.4",
                    "description": "Json.NET", "authors": "James Newton-King",
                    "totalDownloads": 5, "verified": true}]}"#
            } else if uri == "https://packages.example/v3/newtonsoft.json/index.json" {
                r#"{"versions": ["12.0.1", "13.0.1", "13.0.4"]}"#
            } else {
                return Ok(http_client::Response::builder()
                    .status(404)
                    .body(AsyncBody::default())?);
            };
            Ok(http_client::Response::builder()
                .status(200)
                .body(body.into())?)
        });
        let client = NuGetClient::new(http);
        assert_eq!(
            client.search("Newtonsoft Json", false).await.unwrap(),
            vec![SearchResult {
                id: "Newtonsoft.Json".into(),
                version: "13.0.4".into(),
                description: "Json.NET".into(),
                authors: vec!["James Newton-King".into()],
                total_downloads: 5,
                verified: true,
            }]
        );
        assert_eq!(
            client.versions("Newtonsoft.Json").await.unwrap(),
            vec!["13.0.4", "13.0.1", "12.0.1"]
        );
    }

    #[test]
    fn builds_package_commands() {
        let project = Path::new("App.csproj");
        assert_eq!(
            package_command(project, "Serilog", Some("4.0.0")),
            vec![
                "add",
                "App.csproj",
                "package",
                "Serilog",
                "--version",
                "4.0.0"
            ]
        );
        assert_eq!(
            package_command(project, "Serilog", None),
            vec!["remove", "App.csproj", "package", "Serilog"]
        );
        assert!(is_prerelease("1.0.0-beta.1+sha"));
        assert!(!is_prerelease("1.0.0+sha-1"));
    }
}
