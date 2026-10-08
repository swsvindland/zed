use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use quick_xml::events::{BytesStart, Event};
use serde::{Deserialize, Serialize};

pub const PROJECT_EXTENSIONS: &[&str] = &["csproj", "vbproj", "fsproj"];
pub const SOLUTION_EXTENSIONS: &[&str] = &["sln", "slnx"];

/// The project type GUID Visual Studio uses for ASP.NET (System.Web) web application projects.
const WEB_APPLICATION_PROJECT_TYPE: &str = "349c5851-65df-11da-9384-00065b846f21";

/// Item types that refer to assemblies or packages rather than to files in the project.
const REFERENCE_ITEM_TYPES: &[&str] = &[
    "Reference",
    "ProjectReference",
    "PackageReference",
    "COMReference",
    "Analyzer",
];

/// SDKs whose projects default to `<OutputType>Exe</OutputType>`.
const EXECUTABLE_SDKS: &[&str] = &[
    "Microsoft.NET.Sdk.Web",
    "Microsoft.NET.Sdk.Worker",
    "Microsoft.NET.Sdk.BlazorWebAssembly",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectStyle {
    /// A project that `dotnet build` understands (`<Project Sdk="...">`).
    Sdk,
    /// A pre-SDK project (`ToolsVersion`, `packages.config`, ...), which only Visual Studio's
    /// MSBuild.exe can build.
    Legacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputType {
    Library,
    Exe,
    WinExe,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MsBuildProject {
    pub path: PathBuf,
    pub name: String,
    pub style: ProjectStyle,
    pub output_type: OutputType,
    pub assembly_name: String,
    /// Target framework monikers for SDK projects (`net8.0`, `net48`), or the
    /// `TargetFrameworkVersion` (`v4.8`) of legacy projects.
    pub target_frameworks: Vec<String>,
    /// The `OutputPath` of the Debug configuration, for legacy projects.
    pub output_path: Option<String>,
    /// Set for ASP.NET (System.Web) web application projects, which run under IIS Express.
    pub web: Option<LegacyWebProject>,
    /// Names of the `launchSettings.json` profiles that `dotnet run` can start.
    pub launch_profiles: Vec<String>,
    /// The `bin` and `obj` directories, relative to the project directory.
    pub output_directories: Vec<String>,
    /// The files a legacy project lists, relative to the project directory and possibly containing
    /// wildcards. SDK-style projects include the files in their directory instead.
    pub items: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LegacyWebProject {
    pub iis_url: Option<String>,
    pub development_server_port: Option<u16>,
}

impl MsBuildProject {
    pub fn directory(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new(""))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Solution {
    pub path: PathBuf,
    pub name: String,
    pub projects: Vec<PathBuf>,
    /// Files added to the solution itself, such as `Directory.Build.props`.
    pub items: Vec<PathBuf>,
}

impl Solution {
    pub fn directory(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new(""))
    }

    pub fn contains(&self, project_path: &Path) -> bool {
        self.projects
            .iter()
            .any(|path| same_path(path, project_path))
    }
}

pub fn is_project_file(path: &Path) -> bool {
    has_extension(path, PROJECT_EXTENSIONS)
}

pub fn is_solution_file(path: &Path) -> bool {
    has_extension(path, SOLUTION_EXTENSIONS)
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extensions
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

pub fn same_path(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Resolves a path written in an MSBuild or solution file, which always uses `\` separators,
/// against `directory`.
pub fn resolve_msbuild_path(directory: &Path, relative_path: &str) -> PathBuf {
    let relative_path = if cfg!(windows) {
        relative_path.replace('/', "\\")
    } else {
        relative_path.replace('\\', "/")
    };
    let joined = directory.join(relative_path);
    util::paths::normalize_lexically(&joined).unwrap_or(joined)
}

pub fn parse_project(path: &Path, contents: &str) -> Result<MsBuildProject> {
    let document = parse_xml(contents)?;
    let root = document
        .child("Project")
        .context("project file has no <Project> element")?;
    let name = file_stem(path);

    let sdk = root
        .attribute("Sdk")
        .or_else(|| {
            root.children_named("Sdk")
                .find_map(|element| element.attribute("Name"))
        })
        .or_else(|| {
            root.children_named("Import")
                .find_map(|element| element.attribute("Sdk"))
        })
        .map(|sdk| sdk.trim().to_owned());

    let target_frameworks = unconditional_property(root, "TargetFrameworks")
        .map(|frameworks| {
            frameworks
                .split(';')
                .map(str::trim)
                .filter(|framework| !framework.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .or_else(|| {
            unconditional_property(root, "TargetFramework").map(|framework| vec![framework.into()])
        });

    // This mirrors how Roslyn's MSBuild workspace tells SDK-style and legacy projects apart.
    let style = if sdk.is_some() || target_frameworks.is_some() {
        ProjectStyle::Sdk
    } else {
        ProjectStyle::Legacy
    };

    let output_type = match unconditional_property(root, "OutputType") {
        Some(output_type) if output_type.eq_ignore_ascii_case("exe") => OutputType::Exe,
        Some(output_type) if output_type.eq_ignore_ascii_case("winexe") => OutputType::WinExe,
        Some(_) => OutputType::Library,
        None => match &sdk {
            Some(sdk)
                if EXECUTABLE_SDKS
                    .iter()
                    .any(|candidate| sdk.eq_ignore_ascii_case(candidate)) =>
            {
                OutputType::Exe
            }
            _ => OutputType::Library,
        },
    };

    let assembly_name = unconditional_property(root, "AssemblyName")
        .filter(|assembly_name| !assembly_name.contains("$("))
        .map(str::to_owned)
        .unwrap_or_else(|| name.clone());

    let (target_frameworks, output_path, web, items) = match style {
        ProjectStyle::Sdk => (target_frameworks.unwrap_or_default(), None, None, None),
        ProjectStyle::Legacy => {
            let target_framework_version = unconditional_property(root, "TargetFrameworkVersion")
                .map(|version| vec![version.to_owned()])
                .unwrap_or_default();
            (
                target_framework_version,
                Some(debug_output_path(root)),
                legacy_web_project(root),
                Some(item_includes(root)),
            )
        }
    };

    let output_directories = [
        ("BaseOutputPath", "bin\\"),
        ("BaseIntermediateOutputPath", "obj\\"),
    ]
    .into_iter()
    .map(|(property, default)| {
        unconditional_property(root, property)
            .filter(|path| !path.contains("$("))
            .unwrap_or(default)
            .to_owned()
    })
    .collect();

    Ok(MsBuildProject {
        path: path.to_path_buf(),
        name,
        style,
        output_type,
        assembly_name,
        target_frameworks,
        output_path,
        web,
        launch_profiles: Vec::new(),
        output_directories,
        items,
    })
}

fn item_includes(root: &XmlElement) -> Vec<String> {
    root.children_named("ItemGroup")
        .flat_map(|group| &group.children)
        .filter(|item| !REFERENCE_ITEM_TYPES.contains(&item.name.as_str()))
        .filter_map(|item| item.attribute("Include"))
        .flat_map(|include| include.split(';'))
        .map(|include| unescape_msbuild(include.trim()))
        .filter(|include| !include.is_empty() && !include.contains("$(") && !include.contains("@("))
        .collect()
}

/// Decodes MSBuild's `%XX` escapes, such as `%20` for a space or `%3B` for a semicolon.
fn unescape_msbuild(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut unescaped = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                unescaped.push(byte);
                index += 3;
            }
            None => {
                unescaped.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&unescaped).into_owned()
}

/// Returns the last value MSBuild would assign to `name` regardless of the build configuration.
fn unconditional_property<'a>(root: &'a XmlElement, name: &'a str) -> Option<&'a str> {
    root.children_named("PropertyGroup")
        .filter(|group| group.attribute("Condition").is_none())
        .flat_map(|group| group.children_named(name))
        .filter(|property| {
            property
                .attribute("Condition")
                .is_none_or(is_default_value_condition)
        })
        .map(|property| property.text.trim())
        .filter(|value| !value.is_empty())
        .last()
}

/// Matches `'$(Configuration)' == ''`-style conditions, which only provide a default.
fn is_default_value_condition(condition: &str) -> bool {
    condition.replace(' ', "").ends_with("==''")
}

fn debug_output_path(root: &XmlElement) -> String {
    let platform = root
        .children_named("PropertyGroup")
        .filter(|group| group.attribute("Condition").is_none())
        .flat_map(|group| group.children_named("Platform"))
        .map(|platform| platform.text.trim())
        .find(|platform| !platform.is_empty())
        .unwrap_or("AnyCPU");
    let debug_platform_condition = format!("'debug|{}'", platform.to_lowercase());

    let output_path_for = |matches_condition: &dyn Fn(&str) -> bool| {
        root.children_named("PropertyGroup")
            .filter(|group| {
                group.attribute("Condition").is_some_and(|condition| {
                    matches_condition(&condition.replace(' ', "").to_lowercase())
                })
            })
            .flat_map(|group| group.children_named("OutputPath"))
            .map(|output_path| output_path.text.trim())
            .find(|output_path| !output_path.is_empty())
    };

    output_path_for(&|condition| condition.contains(&debug_platform_condition))
        .or_else(|| output_path_for(&|condition| condition.contains("'debug")))
        .or_else(|| unconditional_property(root, "OutputPath"))
        .unwrap_or("bin\\Debug\\")
        .to_owned()
}

fn legacy_web_project(root: &XmlElement) -> Option<LegacyWebProject> {
    let has_web_project_type = unconditional_property(root, "ProjectTypeGuids")
        .is_some_and(|guids| guids.to_lowercase().contains(WEB_APPLICATION_PROJECT_TYPE));
    let imports_web_application_targets = root.children_named("Import").any(|import| {
        import
            .attribute("Project")
            .is_some_and(|project| project.contains("Microsoft.WebApplication.targets"))
    });
    let web_project_properties = root
        .child("ProjectExtensions")
        .and_then(|extensions| extensions.descendant("WebProjectProperties"));

    if !has_web_project_type && !imports_web_application_targets && web_project_properties.is_none()
    {
        return None;
    }

    let web_property = |name: &str| {
        web_project_properties
            .and_then(|properties| properties.child(name))
            .map(|property| property.text.trim())
            .filter(|value| !value.is_empty())
    };
    Some(LegacyWebProject {
        iis_url: web_property("IISUrl").map(str::to_owned),
        development_server_port: web_property("DevelopmentServerPort")
            .and_then(|port| port.parse().ok()),
    })
}

/// Returns the names of the `launchSettings.json` profiles `dotnet run --launch-profile` supports.
pub fn parse_launch_settings(contents: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct LaunchSettings {
        #[serde(default)]
        profiles: serde_json::Map<String, serde_json::Value>,
    }

    let launch_settings: LaunchSettings =
        serde_json_lenient::from_str(contents.trim_start_matches('\u{feff}'))?;
    Ok(launch_settings
        .profiles
        .into_iter()
        .filter(|(_, profile)| {
            profile
                .get("commandName")
                .and_then(|command_name| command_name.as_str())
                .is_some_and(|command_name| command_name == "Project")
        })
        .map(|(name, _)| name)
        .collect())
}

pub fn parse_solution(path: &Path, contents: &str) -> Result<Solution> {
    let is_slnx = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("slnx"));
    let (project_paths, item_paths) = if is_slnx {
        slnx_paths(contents)?
    } else {
        sln_paths(contents)
    };
    let directory = path.parent().unwrap_or(Path::new(""));
    Ok(Solution {
        path: path.to_path_buf(),
        name: file_stem(path),
        projects: project_paths
            .iter()
            .map(|relative_path| resolve_msbuild_path(directory, relative_path))
            .filter(|path| is_project_file(path))
            .collect(),
        items: item_paths
            .iter()
            .map(|relative_path| resolve_msbuild_path(directory, relative_path))
            .collect(),
    })
}

/// Extracts the paths of `Project("{type}") = "Name", "Path\Name.csproj", "{guid}"` lines, and
/// of the files listed in `ProjectSection(SolutionItems)` sections.
fn sln_paths(contents: &str) -> (Vec<String>, Vec<String>) {
    let mut project_paths = Vec::new();
    let mut item_paths = Vec::new();
    let mut in_solution_items = false;
    for line in contents.lines() {
        let line = line.trim();
        if in_solution_items {
            if line == "EndProjectSection" {
                in_solution_items = false;
            } else if let Some((path, _)) = line.split_once('=') {
                item_paths.push(path.trim().to_owned());
            }
        } else if line.starts_with("ProjectSection(SolutionItems)") {
            in_solution_items = true;
        } else if let Some(declaration) = line.strip_prefix("Project(")
            && let Some((_, values)) = declaration.split_once('=')
        {
            let mut values = values
                .split(',')
                .map(|value| value.trim().trim_matches('"'));
            let _name = values.next();
            if let Some(path) = values.next() {
                project_paths.push(path.to_owned());
            }
        }
    }
    (project_paths, item_paths)
}

fn slnx_paths(contents: &str) -> Result<(Vec<String>, Vec<String>)> {
    fn collect(
        element: &XmlElement,
        project_paths: &mut Vec<String>,
        item_paths: &mut Vec<String>,
    ) {
        for child in &element.children {
            match (child.name.as_str(), child.attribute("Path")) {
                ("Project", Some(path)) => project_paths.push(path.to_owned()),
                ("File", Some(path)) => item_paths.push(path.to_owned()),
                _ => collect(child, project_paths, item_paths),
            }
        }
    }

    let document = parse_xml(contents)?;
    let mut project_paths = Vec::new();
    let mut item_paths = Vec::new();
    collect(&document, &mut project_paths, &mut item_paths);
    Ok((project_paths, item_paths))
}

/// Finds the IIS Express site serving `project` in an `applicationhost.config` file written by
/// Visual Studio or Rider.
pub fn find_iis_express_site_name(
    config: &str,
    project: &MsBuildProject,
) -> Result<Option<String>> {
    fn comparable(path: &str) -> String {
        path.replace('\\', "/").trim_end_matches('/').to_lowercase()
    }

    let document = parse_xml(config)?;
    let Some(sites) = document
        .child("configuration")
        .and_then(|configuration| configuration.child("system.applicationHost"))
        .and_then(|application_host| application_host.child("sites"))
    else {
        return Ok(None);
    };

    let project_directory = comparable(&project.directory().to_string_lossy());
    let serves_project = |site: &XmlElement| {
        site.children_named("application")
            .filter(|application| application.attribute("path") == Some("/"))
            .flat_map(|application| application.children_named("virtualDirectory"))
            .filter(|directory| directory.attribute("path") == Some("/"))
            .filter_map(|directory| directory.attribute("physicalPath"))
            .any(|physical_path| comparable(physical_path) == project_directory)
    };

    let sites = sites.children_named("site").collect::<Vec<_>>();
    Ok(sites
        .iter()
        .find(|site| serves_project(site))
        .or_else(|| {
            sites.iter().find(|site| {
                site.attribute("name")
                    .is_some_and(|name| name.eq_ignore_ascii_case(&project.name))
            })
        })
        .and_then(|site| site.attribute("name"))
        .map(str::to_owned))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConfiguration {
    pub project_path: PathBuf,
    pub project_name: String,
    pub kind: RunKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunKind {
    /// `dotnet run`, optionally with a `launchSettings.json` profile.
    DotnetRun {
        launch_profile: Option<String>,
        framework: Option<String>,
    },
    /// Builds a legacy project with MSBuild.exe and starts the executable it produces.
    Executable,
    /// Builds an ASP.NET (System.Web) project with MSBuild.exe and serves it with IIS Express.
    IisExpress,
}

impl RunConfiguration {
    pub fn label(&self) -> String {
        let details = match &self.kind {
            RunKind::DotnetRun {
                launch_profile,
                framework,
            } => launch_profile
                .iter()
                .filter(|profile| **profile != self.project_name)
                .chain(framework)
                .map(String::as_str)
                .collect::<Vec<_>>(),
            RunKind::Executable => Vec::new(),
            RunKind::IisExpress => vec!["IIS Express"],
        };
        if details.is_empty() {
            self.project_name.clone()
        } else {
            format!("{} ({})", self.project_name, details.join(", "))
        }
    }
}

pub fn run_configurations(project: &MsBuildProject, is_windows: bool) -> Vec<RunConfiguration> {
    let configuration = |kind| RunConfiguration {
        project_path: project.path.clone(),
        project_name: project.name.clone(),
        kind,
    };

    match project.style {
        ProjectStyle::Legacy => {
            if !is_windows {
                Vec::new()
            } else if project.web.is_some() {
                vec![configuration(RunKind::IisExpress)]
            } else if project.output_type != OutputType::Library {
                vec![configuration(RunKind::Executable)]
            } else {
                Vec::new()
            }
        }
        ProjectStyle::Sdk => {
            if project.output_type == OutputType::Library {
                return Vec::new();
            }
            let frameworks = if project.target_frameworks.len() > 1 {
                let frameworks = project
                    .target_frameworks
                    .iter()
                    .filter(|framework| is_windows || !is_net_framework(framework))
                    .map(|framework| Some(framework.clone()))
                    .collect::<Vec<_>>();
                if frameworks.is_empty() {
                    return Vec::new();
                }
                frameworks
            } else {
                if !is_windows
                    && project
                        .target_frameworks
                        .iter()
                        .any(|framework| is_net_framework(framework))
                {
                    return Vec::new();
                }
                vec![None]
            };
            let launch_profiles = if project.launch_profiles.is_empty() {
                vec![None]
            } else {
                project.launch_profiles.iter().cloned().map(Some).collect()
            };

            let mut configurations = Vec::new();
            for launch_profile in &launch_profiles {
                for framework in &frameworks {
                    configurations.push(configuration(RunKind::DotnetRun {
                        launch_profile: launch_profile.clone(),
                        framework: framework.clone(),
                    }));
                }
            }
            configurations
        }
    }
}

/// Whether `framework` is a .NET Framework moniker (`net48`, `net472`), which only runs on Windows.
fn is_net_framework(framework: &str) -> bool {
    framework
        .strip_prefix("net")
        .is_some_and(|version| !version.is_empty() && version.chars().all(|c| c.is_ascii_digit()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandLine {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildVerb {
    Build,
    Rebuild,
    Clean,
}

impl BuildVerb {
    pub fn label(self) -> &'static str {
        match self {
            BuildVerb::Build => "Build",
            BuildVerb::Rebuild => "Rebuild",
            BuildVerb::Clean => "Clean",
        }
    }
}

pub fn dotnet_build_command(target: &Path, verb: BuildVerb) -> CommandLine {
    let target = target.to_string_lossy().into_owned();
    let args = match verb {
        BuildVerb::Build => vec!["build".into(), target],
        BuildVerb::Rebuild => vec!["build".into(), target, "--no-incremental".into()],
        BuildVerb::Clean => vec!["clean".into(), target],
    };
    CommandLine {
        program: "dotnet".into(),
        args,
    }
}

/// Builds `target` (a project or solution) with Visual Studio's MSBuild.exe, restoring
/// `packages.config` packages the way Visual Studio does.
pub fn msbuild_command(
    msbuild: &Path,
    target: &Path,
    solution_directory: Option<&Path>,
    verb: BuildVerb,
) -> CommandLine {
    let mut args = vec![
        target.to_string_lossy().into_owned(),
        "-nologo".into(),
        "-verbosity:minimal".into(),
        "-maxCpuCount".into(),
        "-p:Configuration=Debug".into(),
    ];
    match verb {
        BuildVerb::Build => {
            args.push("-restore".into());
            args.push("-p:RestorePackagesConfig=true".into());
        }
        BuildVerb::Rebuild => {
            args.push("-restore".into());
            args.push("-p:RestorePackagesConfig=true".into());
            args.push("-t:Rebuild".into());
        }
        BuildVerb::Clean => args.push("-t:Clean".into()),
    }
    // Visual Studio defines `SolutionDir` when building a single project, and legacy projects
    // rely on it to find the solution-level `packages` folder. The trailing `/` (rather than
    // `\`) keeps the closing quote from being escaped when the path contains spaces.
    if !is_solution_file(target)
        && let Some(solution_directory) = solution_directory
    {
        args.push(format!(
            "-p:SolutionDir={}/",
            solution_directory.to_string_lossy()
        ));
    }
    CommandLine {
        program: msbuild.to_string_lossy().into_owned(),
        args,
    }
}

pub fn dotnet_run_command(
    project: &MsBuildProject,
    launch_profile: Option<&str>,
    framework: Option<&str>,
) -> CommandLine {
    let mut args = vec![
        "run".into(),
        "--project".into(),
        project.path.to_string_lossy().into_owned(),
    ];
    if let Some(launch_profile) = launch_profile {
        args.push("--launch-profile".into());
        args.push(launch_profile.into());
    }
    if let Some(framework) = framework {
        args.push("--framework".into());
        args.push(framework.into());
    }
    CommandLine {
        program: "dotnet".into(),
        args,
    }
}

pub fn executable_output_directory(project: &MsBuildProject) -> PathBuf {
    resolve_msbuild_path(
        project.directory(),
        project.output_path.as_deref().unwrap_or("bin\\Debug\\"),
    )
}

pub fn executable_path(project: &MsBuildProject) -> PathBuf {
    executable_output_directory(project).join(format!("{}.exe", project.assembly_name))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IisExpressSite {
    pub config_path: PathBuf,
    pub name: String,
}

pub fn iis_express_command(
    iis_express: &Path,
    project: &MsBuildProject,
    site: Option<&IisExpressSite>,
) -> CommandLine {
    let args = match site {
        Some(site) => vec![
            format!("/config:{}", site.config_path.to_string_lossy()),
            format!("/site:{}", site.name),
        ],
        None => {
            // Without a site definition from Visual Studio or Rider, IIS Express can still serve
            // the project directory over plain HTTP.
            let port = project
                .web
                .as_ref()
                .and_then(|web| {
                    web.iis_url
                        .as_deref()
                        .and_then(|iis_url| url::Url::parse(iis_url).ok())
                        .filter(|iis_url| iis_url.scheme() == "http")
                        .and_then(|iis_url| iis_url.port())
                        .or(web.development_server_port)
                })
                .unwrap_or(8080);
            let clr_version = if project
                .target_frameworks
                .iter()
                .any(|version| version.starts_with("v2") || version.starts_with("v3"))
            {
                "v2.0"
            } else {
                "v4.0"
            };
            vec![
                format!("/path:{}", project.directory().to_string_lossy()),
                format!("/port:{port}"),
                format!("/clr:{clr_version}"),
            ]
        }
    };
    CommandLine {
        program: iis_express.to_string_lossy().into_owned(),
        args,
    }
}

#[derive(Debug, Default)]
struct XmlElement {
    name: String,
    attributes: Vec<(String, String)>,
    text: String,
    children: Vec<XmlElement>,
}

impl XmlElement {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn child(&self, name: &str) -> Option<&XmlElement> {
        self.children.iter().find(|child| child.name == name)
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlElement> {
        self.children.iter().filter(move |child| child.name == name)
    }

    fn descendant(&self, name: &str) -> Option<&XmlElement> {
        self.children.iter().find_map(|child| {
            if child.name == name {
                Some(child)
            } else {
                child.descendant(name)
            }
        })
    }
}

/// Parses an XML document into a tree whose root holds the document's top-level elements as
/// children.
fn parse_xml(xml: &str) -> Result<XmlElement> {
    fn element(start: &BytesStart) -> Result<XmlElement> {
        let mut element = XmlElement {
            name: String::from_utf8_lossy(start.name().as_ref()).into_owned(),
            ..Default::default()
        };
        for attribute in start.attributes() {
            let attribute = attribute?;
            element.attributes.push((
                String::from_utf8_lossy(attribute.key.as_ref()).into_owned(),
                attribute
                    .normalized_value(quick_xml::XmlVersion::Explicit1_0)?
                    .into_owned(),
            ));
        }
        Ok(element)
    }

    let mut reader = quick_xml::Reader::from_str(xml.trim_start_matches('\u{feff}'));
    let mut stack = vec![XmlElement::default()];
    loop {
        match reader.read_event()? {
            Event::Start(start) => stack.push(element(&start)?),
            Event::Empty(start) => {
                let element = element(&start)?;
                stack
                    .last_mut()
                    .context("unbalanced XML")?
                    .children
                    .push(element);
            }
            Event::End(_) => {
                anyhow::ensure!(stack.len() > 1, "unbalanced XML");
                let element = stack.pop().context("unbalanced XML")?;
                stack
                    .last_mut()
                    .context("unbalanced XML")?
                    .children
                    .push(element);
            }
            Event::Text(text) => {
                let text = text.xml10_content()?;
                if let Some(element) = stack.last_mut() {
                    element.text.push_str(&text);
                }
            }
            Event::CData(data) => {
                let text = data.decode()?;
                if let Some(element) = stack.last_mut() {
                    element.text.push_str(&text);
                }
            }
            Event::GeneralRef(reference) => {
                let text = match reference.resolve_char_ref()? {
                    Some(character) => character.to_string(),
                    None => {
                        let name = reference.decode()?;
                        quick_xml::escape::resolve_predefined_entity(&name)
                            .with_context(|| format!("unknown XML entity {name}"))?
                            .to_string()
                    }
                };
                if let Some(element) = stack.last_mut() {
                    element.text.push_str(&text);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    anyhow::ensure!(stack.len() == 1, "unbalanced XML");
    stack.pop().context("empty XML document")
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    fn root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from("C:\\work\\App")
        } else {
            PathBuf::from("/work/App")
        }
    }

    const LEGACY_WEB_PROJECT: &str = indoc! {r#"
        <?xml version="1.0" encoding="utf-8"?>
        <Project ToolsVersion="15.0" DefaultTargets="Build" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
          <Import Project="..\packages\Microsoft.CodeDom.Providers.DotNetCompilerPlatform.2.0.1\build\net46\Microsoft.CodeDom.Providers.DotNetCompilerPlatform.props" Condition="Exists('..\packages\Microsoft.CodeDom.Providers.DotNetCompilerPlatform.2.0.1\build\net46\Microsoft.CodeDom.Providers.DotNetCompilerPlatform.props')" />
          <Import Project="$(MSBuildExtensionsPath)\$(MSBuildToolsVersion)\Microsoft.Common.props" Condition="Exists('$(MSBuildExtensionsPath)\$(MSBuildToolsVersion)\Microsoft.Common.props')" />
          <PropertyGroup>
            <Configuration Condition=" '$(Configuration)' == '' ">Debug</Configuration>
            <Platform Condition=" '$(Platform)' == '' ">AnyCPU</Platform>
            <ProjectTypeGuids>{349c5851-65df-11da-9384-00065b846f21};{fae04ec0-301f-11d3-bf4b-00c04f79efbc}</ProjectTypeGuids>
            <OutputType>Library</OutputType>
            <RootNamespace>Shop.Web</RootNamespace>
            <AssemblyName>Shop.Web</AssemblyName>
            <TargetFrameworkVersion>v4.7.2</TargetFrameworkVersion>
            <UseIISExpress>true</UseIISExpress>
            <IISExpressSSLPort>44300</IISExpressSSLPort>
          </PropertyGroup>
          <PropertyGroup Condition=" '$(Configuration)|$(Platform)' == 'Debug|AnyCPU' ">
            <DebugSymbols>true</DebugSymbols>
            <OutputPath>bin\</OutputPath>
          </PropertyGroup>
          <PropertyGroup Condition=" '$(Configuration)|$(Platform)' == 'Release|AnyCPU' ">
            <OutputPath>bin\</OutputPath>
          </PropertyGroup>
          <ItemGroup>
            <Compile Include="Global.asax.cs">
              <DependentUpon>Global.asax</DependentUpon>
            </Compile>
          </ItemGroup>
          <Import Project="$(MSBuildBinPath)\Microsoft.CSharp.targets" />
          <Import Project="$(VSToolsPath)\WebApplications\Microsoft.WebApplication.targets" Condition="'$(VSToolsPath)' != ''" />
          <ProjectExtensions>
            <VisualStudio>
              <FlavorProperties GUID="{349c5851-65df-11da-9384-00065b846f21}">
                <WebProjectProperties>
                  <UseIIS>True</UseIIS>
                  <AutoAssignPort>True</AutoAssignPort>
                  <DevelopmentServerPort>51234</DevelopmentServerPort>
                  <DevelopmentServerVPath>/</DevelopmentServerVPath>
                  <IISUrl>https://localhost:44300/</IISUrl>
                </WebProjectProperties>
              </FlavorProperties>
            </VisualStudio>
          </ProjectExtensions>
        </Project>
    "#};

    #[test]
    fn parses_legacy_web_project() {
        let path = root().join("Shop.Web").join("Shop.Web.csproj");
        let project = parse_project(&path, &format!("\u{feff}{LEGACY_WEB_PROJECT}")).unwrap();
        assert_eq!(
            project,
            MsBuildProject {
                path,
                name: "Shop.Web".into(),
                style: ProjectStyle::Legacy,
                output_type: OutputType::Library,
                assembly_name: "Shop.Web".into(),
                target_frameworks: vec!["v4.7.2".into()],
                output_path: Some("bin\\".into()),
                web: Some(LegacyWebProject {
                    iis_url: Some("https://localhost:44300/".into()),
                    development_server_port: Some(51234),
                }),
                launch_profiles: Vec::new(),
                output_directories: vec!["bin\\".into(), "obj\\".into()],
                items: Some(vec!["Global.asax.cs".into()]),
            }
        );
        assert_eq!(
            run_configurations(&project, true)
                .iter()
                .map(RunConfiguration::label)
                .collect::<Vec<_>>(),
            vec!["Shop.Web (IIS Express)"]
        );
        assert_eq!(run_configurations(&project, false), Vec::new());
    }

    #[test]
    fn parses_items_and_output_directories() {
        let legacy = parse_project(
            &root().join("Legacy").join("Legacy.csproj"),
            indoc! {r#"
                <Project ToolsVersion="15.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
                  <ItemGroup>
                    <Reference Include="System.Web" />
                    <Compile Include="Default.aspx.cs;Site%20Master.cs" />
                    <Content Include="Scripts\**\*.js" />
                    <Folder Include="App_Data\" />
                    <ProjectReference Include="..\Core\Core.csproj" />
                  </ItemGroup>
                </Project>
            "#},
        )
        .unwrap();
        assert_eq!(
            legacy.items,
            Some(vec![
                "Default.aspx.cs".into(),
                "Site Master.cs".into(),
                "Scripts\\**\\*.js".into(),
                "App_Data\\".into(),
            ])
        );

        let sdk = parse_project(
            &root().join("Api").join("Api.csproj"),
            indoc! {r#"
                <Project Sdk="Microsoft.NET.Sdk">
                  <PropertyGroup>
                    <TargetFramework>net8.0</TargetFramework>
                    <BaseOutputPath>build\</BaseOutputPath>
                  </PropertyGroup>
                </Project>
            "#},
        )
        .unwrap();
        assert_eq!(sdk.items, None);
        assert_eq!(sdk.output_directories, vec!["build\\", "obj\\"]);
    }

    #[test]
    fn parses_legacy_console_project() {
        let path = root().join("Tool").join("Tool.csproj");
        let project = parse_project(
            &path,
            indoc! {r#"
                <Project ToolsVersion="15.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
                  <PropertyGroup>
                    <Configuration Condition=" '$(Configuration)' == '' ">Debug</Configuration>
                    <Platform Condition=" '$(Platform)' == '' ">x86</Platform>
                    <OutputType>Exe</OutputType>
                    <AssemblyName>tool</AssemblyName>
                    <TargetFrameworkVersion>v4.8</TargetFrameworkVersion>
                  </PropertyGroup>
                  <PropertyGroup Condition=" '$(Configuration)|$(Platform)' == 'Debug|AnyCPU' ">
                    <OutputPath>bin\AnyCpu\Debug\</OutputPath>
                  </PropertyGroup>
                  <PropertyGroup Condition=" '$(Configuration)|$(Platform)' == 'Debug|x86' ">
                    <OutputPath>bin\x86\Debug\</OutputPath>
                  </PropertyGroup>
                  <Import Project="$(MSBuildToolsPath)\Microsoft.CSharp.targets" />
                </Project>
            "#},
        )
        .unwrap();
        assert_eq!(project.style, ProjectStyle::Legacy);
        assert_eq!(project.output_type, OutputType::Exe);
        assert_eq!(project.web, None);
        assert_eq!(
            executable_path(&project),
            resolve_msbuild_path(&root().join("Tool"), "bin\\x86\\Debug\\tool.exe")
        );
        assert_eq!(
            run_configurations(&project, true),
            vec![RunConfiguration {
                project_path: path,
                project_name: "Tool".into(),
                kind: RunKind::Executable,
            }]
        );
    }

    #[test]
    fn parses_sdk_projects() {
        let web = parse_project(
            &root().join("Api").join("Api.csproj"),
            indoc! {r#"
                <Project Sdk="Microsoft.NET.Sdk.Web">
                  <PropertyGroup>
                    <TargetFramework>net8.0</TargetFramework>
                  </PropertyGroup>
                </Project>
            "#},
        )
        .unwrap();
        assert_eq!(web.style, ProjectStyle::Sdk);
        assert_eq!(web.output_type, OutputType::Exe);
        assert_eq!(web.target_frameworks, vec!["net8.0"]);
        assert_eq!(web.output_path, None);

        let library = parse_project(
            &root().join("Core").join("Core.csproj"),
            indoc! {r#"
                <Project>
                  <Sdk Name="Microsoft.NET.Sdk" />
                  <PropertyGroup>
                    <TargetFrameworks>netstandard2.0;net48</TargetFrameworks>
                  </PropertyGroup>
                </Project>
            "#},
        )
        .unwrap();
        assert_eq!(library.style, ProjectStyle::Sdk);
        assert_eq!(library.output_type, OutputType::Library);
        assert_eq!(library.target_frameworks, vec!["netstandard2.0", "net48"]);
        assert_eq!(run_configurations(&library, true), Vec::new());
    }

    #[test]
    fn sdk_run_configurations() {
        let mut project = parse_project(
            &root().join("App").join("App.csproj"),
            indoc! {r#"
                <Project Sdk="Microsoft.NET.Sdk">
                  <PropertyGroup>
                    <OutputType>WinExe</OutputType>
                    <TargetFrameworks>net48;net8.0-windows</TargetFrameworks>
                  </PropertyGroup>
                </Project>
            "#},
        )
        .unwrap();
        project.launch_profiles = parse_launch_settings(&format!(
            "\u{feff}{}",
            indoc! {r#"
            {
              // Comments are allowed, as in Visual Studio.
              "profiles": {
                "App": { "commandName": "Project" },
                "IIS Express": { "commandName": "IISExpress" },
                "Staging": { "commandName": "Project", "environmentVariables": {} },
              }
            }
        "#}
        ))
        .unwrap();

        fn labels(project: &MsBuildProject, is_windows: bool) -> Vec<String> {
            run_configurations(project, is_windows)
                .iter()
                .map(RunConfiguration::label)
                .collect()
        }
        assert_eq!(
            labels(&project, true),
            vec![
                "App (net48)",
                "App (net8.0-windows)",
                "App (Staging, net48)",
                "App (Staging, net8.0-windows)",
            ]
        );
        assert_eq!(
            labels(&project, false),
            vec!["App (net8.0-windows)", "App (Staging, net8.0-windows)"]
        );

        project.target_frameworks = vec!["net48".into()];
        assert_eq!(labels(&project, false), Vec::<String>::new());
        assert_eq!(labels(&project, true), vec!["App", "App (Staging)"]);
    }

    #[test]
    fn parses_solutions() {
        let sln = parse_solution(
            &root().join("App.sln"),
            indoc! {r#"
                Microsoft Visual Studio Solution File, Format Version 12.00
                # Visual Studio Version 17
                Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Shop.Web", "Shop.Web\Shop.Web.csproj", "{5B3C1A31-4C8D-4D7A-9F8A-1B3C4D5E6F70}"
                EndProject
                Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "Solution Items", "Solution Items", "{8C1E2D3F-4A5B-6C7D-8E9F-0A1B2C3D4E5F}"
                    ProjectSection(SolutionItems) = preProject
                        Directory.Build.props = Directory.Build.props
                        build\common.props = build\common.props
                    EndProjectSection
                EndProject
                Project("{9A19103F-16F7-4668-BE54-9A1E7A4F7556}") = "Shared", "..\Shared\Shared.csproj", "{1A2B3C4D-5E6F-7081-92A3-B4C5D6E7F809}"
                EndProject
                Global
                EndGlobal
            "#},
        )
        .unwrap();
        assert_eq!(sln.name, "App");
        assert_eq!(
            sln.projects,
            vec![
                root().join("Shop.Web").join("Shop.Web.csproj"),
                root()
                    .parent()
                    .unwrap()
                    .join("Shared")
                    .join("Shared.csproj"),
            ]
        );
        assert!(sln.contains(&root().join("Shop.Web").join("Shop.Web.csproj")));
        assert_eq!(
            sln.items,
            vec![
                root().join("Directory.Build.props"),
                root().join("build").join("common.props"),
            ]
        );

        let slnx = parse_solution(
            &root().join("App.slnx"),
            indoc! {r#"
                <Solution>
                  <Folder Name="/src/">
                    <Project Path="src/Api/Api.csproj" />
                  </Folder>
                  <Project Path="tests\Api.Tests\Api.Tests.csproj" />
                  <Folder Name="/Solution Items/">
                    <File Path="global.json" />
                  </Folder>
                </Solution>
            "#},
        )
        .unwrap();
        assert_eq!(
            slnx.projects,
            vec![
                root().join("src").join("Api").join("Api.csproj"),
                root()
                    .join("tests")
                    .join("Api.Tests")
                    .join("Api.Tests.csproj"),
            ]
        );
        assert_eq!(slnx.items, vec![root().join("global.json")]);
    }

    #[test]
    fn finds_iis_express_site() {
        let path = root().join("Shop.Web").join("Shop.Web.csproj");
        let project = parse_project(&path, LEGACY_WEB_PROJECT).unwrap();
        let physical_path = project.directory().to_string_lossy().replace('/', "\\");
        let config = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <configuration>
              <system.applicationHost>
                <sites>
                  <site name="WebSite1" id="1" serverAutoStart="true">
                    <application path="/">
                      <virtualDirectory path="/" physicalPath="%IIS_SITES_HOME%\WebSite1" />
                    </application>
                  </site>
                  <site name="Shop.Web(1)" id="2">
                    <application path="/" applicationPool="Clr4IntegratedAppPool">
                      <virtualDirectory path="/" physicalPath="{physical_path}\" />
                    </application>
                    <bindings>
                      <binding protocol="https" bindingInformation="*:44300:localhost" />
                    </bindings>
                  </site>
                </sites>
              </system.applicationHost>
            </configuration>"#
        );
        assert_eq!(
            find_iis_express_site_name(&config, &project).unwrap(),
            Some("Shop.Web(1)".into())
        );
        assert_eq!(
            find_iis_express_site_name("<configuration />", &project).unwrap(),
            None
        );
    }

    #[test]
    fn builds_commands() {
        let path = root().join("Shop.Web").join("Shop.Web.csproj");
        let project = parse_project(&path, LEGACY_WEB_PROJECT).unwrap();
        let msbuild = PathBuf::from("MSBuild.exe");
        let solution_directory = root();

        assert_eq!(
            msbuild_command(&msbuild, &path, Some(&solution_directory), BuildVerb::Build).args,
            vec![
                path.to_string_lossy().into_owned(),
                "-nologo".into(),
                "-verbosity:minimal".into(),
                "-maxCpuCount".into(),
                "-p:Configuration=Debug".into(),
                "-restore".into(),
                "-p:RestorePackagesConfig=true".into(),
                format!("-p:SolutionDir={}/", root().to_string_lossy()),
            ]
        );
        assert_eq!(
            msbuild_command(
                &msbuild,
                &root().join("App.sln"),
                Some(&solution_directory),
                BuildVerb::Clean
            )
            .args
            .last()
            .map(String::as_str),
            Some("-t:Clean")
        );

        let iis_express = PathBuf::from("iisexpress.exe");
        assert_eq!(
            iis_express_command(&iis_express, &project, None).args,
            vec![
                format!("/path:{}", project.directory().to_string_lossy()),
                "/port:51234".into(),
                "/clr:v4.0".into(),
            ]
        );
        let site = IisExpressSite {
            config_path: root().join("applicationhost.config"),
            name: "Shop.Web".into(),
        };
        assert_eq!(
            iis_express_command(&iis_express, &project, Some(&site)).args,
            vec![
                format!(
                    "/config:{}",
                    root().join("applicationhost.config").to_string_lossy()
                ),
                "/site:Shop.Web".into(),
            ]
        );

        assert_eq!(
            dotnet_run_command(&project, Some("https"), Some("net8.0")).args,
            vec![
                "run".to_string(),
                "--project".into(),
                path.to_string_lossy().into_owned(),
                "--launch-profile".into(),
                "https".into(),
                "--framework".into(),
                "net8.0".into(),
            ]
        );
    }
}
