// Adapted from https://github.com/zed-extensions/csharp (Apache-2.0).

use std::cmp::Ordering;

use zed_extension_api::{self as zed, Result, http_client, serde_json};

const NUGET_SERVICE_INDEX: &str = "https://api.nuget.org/v3/index.json";

#[derive(Default)]
pub struct NuGetClient {
    package_base_address: Option<String>,
}

impl NuGetClient {
    fn package_base_address(&mut self) -> Result<String> {
        if let Some(base_address) = &self.package_base_address {
            return Ok(base_address.clone());
        }

        let index = fetch_json(NUGET_SERVICE_INDEX)?;
        let base_address = index["resources"]
            .as_array()
            .ok_or("invalid NuGet service index: missing 'resources'")?
            .iter()
            .find(|resource| resource["@type"].as_str() == Some("PackageBaseAddress/3.0.0"))
            .and_then(|resource| resource["@id"].as_str())
            .ok_or("PackageBaseAddress/3.0.0 not found in NuGet service index")?
            .trim_end_matches('/')
            .to_string();

        self.package_base_address = Some(base_address.clone());
        Ok(base_address)
    }

    pub fn latest_version(&mut self, package_id: &str) -> Result<String> {
        let base_address = self.package_base_address()?;
        let lower_id = package_id.to_lowercase();
        let body = fetch_json(&format!("{base_address}/{lower_id}/index.json"))?;

        body["versions"]
            .as_array()
            .ok_or_else(|| format!("no versions listed for NuGet package '{package_id}'"))?
            .iter()
            .filter_map(|version| version.as_str())
            .filter_map(NuGetVersion::parse)
            .max()
            .map(|version| version.raw)
            .ok_or_else(|| format!("no parseable versions for NuGet package '{package_id}'"))
    }

    pub fn download_and_extract(
        &mut self,
        package_id: &str,
        version: &str,
        destination: &str,
    ) -> Result<()> {
        let base_address = self.package_base_address()?;
        let lower_id = package_id.to_lowercase();
        let lower_version = version.to_lowercase();
        let url =
            format!("{base_address}/{lower_id}/{lower_version}/{lower_id}.{lower_version}.nupkg");

        zed::download_file(&url, destination, zed::DownloadedFileType::Zip)
            .map_err(|error| format!("failed to download {package_id} {version}: {error}"))
    }
}

fn fetch_json(url: &str) -> Result<serde_json::Value> {
    let response = http_client::fetch(
        &http_client::HttpRequest::builder()
            .method(http_client::HttpMethod::Get)
            .url(url)
            .redirect_policy(http_client::RedirectPolicy::FollowAll)
            .build()?,
    )?;
    serde_json::from_slice(&response.body)
        .map_err(|error| format!("invalid JSON from {url}: {error}"))
}

#[derive(Debug, Clone)]
struct NuGetVersion {
    numbers: [u64; 4],
    prerelease: Option<String>,
    raw: String,
}

impl NuGetVersion {
    fn parse(input: &str) -> Option<Self> {
        let (core, prerelease) = match input.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease.to_string())),
            None => (input, None),
        };
        let segments = core
            .split('.')
            .map(|segment| segment.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        if segments.is_empty() || segments.len() > 4 {
            return None;
        }
        let mut numbers = [0; 4];
        numbers[..segments.len()].copy_from_slice(&segments);
        Some(Self {
            numbers,
            prerelease,
            raw: input.to_string(),
        })
    }
}

fn compare_prerelease_identifier(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase()),
    }
}

impl Ord for NuGetVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers
            .cmp(&other.numbers)
            .then_with(|| match (&self.prerelease, &other.prerelease) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => {
                    let mut left_parts = left.split('.');
                    let mut right_parts = right.split('.');
                    loop {
                        match (left_parts.next(), right_parts.next()) {
                            (Some(left), Some(right)) => {
                                let ordering = compare_prerelease_identifier(left, right);
                                if ordering != Ordering::Equal {
                                    return ordering;
                                }
                            }
                            (None, Some(_)) => return Ordering::Less,
                            (Some(_), None) => return Ordering::Greater,
                            (None, None) => return Ordering::Equal,
                        }
                    }
                }
            })
    }
}

impl PartialOrd for NuGetVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for NuGetVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for NuGetVersion {}
