use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path},
    sync::Arc,
};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use gpui::SharedString;
use project::WorktreeId;
use project_panel::ProjectView;
use util::{ResultExt as _, rel_path::RelPath};

use crate::msbuild::{MsBuildProject, Solution};

/// The files that belong to the workspace's solutions and projects, as in the solution view of
/// Visual Studio or Rider: build outputs, files that legacy projects don't list, and files that
/// aren't part of any project or solution are left out.
pub(crate) struct SolutionView {
    worktrees: HashMap<WorktreeId, WorktreeContents>,
}

#[derive(Default)]
struct WorktreeContents {
    /// Solution files, solution items, project directories, and the directories leading to them.
    paths: HashSet<String>,
    projects: Vec<ProjectContents>,
}

struct ProjectContents {
    directory: String,
    output_directories: Vec<String>,
    /// For legacy projects, the files the project lists. Other projects include every file in
    /// their directory.
    items: Option<ProjectItems>,
}

struct ProjectItems {
    /// The listed files and the directories containing them.
    paths: HashSet<String>,
    globs: GlobSet,
    /// The directories that wildcard items like `Scripts\**\*.js` start from.
    glob_directories: Vec<String>,
}

impl SolutionView {
    pub(crate) fn new(
        worktrees: &[(WorktreeId, Arc<Path>)],
        solutions: &[Solution],
        projects: &[MsBuildProject],
    ) -> Option<Self> {
        let mut contents = HashMap::<WorktreeId, WorktreeContents>::default();
        for solution in solutions {
            for path in std::iter::once(&solution.path).chain(&solution.items) {
                if let Some((worktree_id, path)) = worktree_relative_path(worktrees, path) {
                    add_with_ancestors(&mut contents.entry(worktree_id).or_default().paths, &path);
                }
            }
        }
        for project in projects {
            let Some((worktree_id, directory)) =
                worktree_relative_path(worktrees, project.directory())
            else {
                continue;
            };
            let worktree = contents.entry(worktree_id).or_default();
            add_with_ancestors(&mut worktree.paths, &directory);
            worktree
                .projects
                .push(ProjectContents::new(directory, project));
        }
        (!contents.is_empty()).then_some(Self {
            worktrees: contents,
        })
    }
}

impl ProjectView for SolutionView {
    fn name(&self) -> SharedString {
        "Solution".into()
    }

    fn contains(&self, worktree_id: WorktreeId, path: &RelPath, is_dir: bool) -> bool {
        let Some(worktree) = self.worktrees.get(&worktree_id) else {
            return true;
        };
        let path = path_key(path.as_unix_str());
        let project = worktree
            .projects
            .iter()
            .filter(|project| is_within(&path, &project.directory))
            .max_by_key(|project| project.directory.len());
        match project {
            Some(project) => project.contains(relative_to(&path, &project.directory), is_dir),
            None => worktree.paths.contains(&path),
        }
    }
}

impl ProjectContents {
    fn new(directory: String, project: &MsBuildProject) -> Self {
        let output_directories = project
            .output_directories
            .iter()
            .filter_map(|output_directory| project_relative_path(output_directory))
            .collect();

        let items = project.items.as_ref().map(|items| {
            let mut paths = HashSet::default();
            let mut globs = GlobSetBuilder::new();
            let mut glob_directories = Vec::new();
            if let Some(file_name) = project.path.file_name() {
                paths.insert(path_key(&file_name.to_string_lossy()));
            }
            for item in items {
                let Some(item) = project_relative_path(item) else {
                    continue;
                };
                if item.contains(['*', '?']) {
                    let directory = item
                        .split('/')
                        .take_while(|component| !component.contains(['*', '?']))
                        .collect::<Vec<_>>()
                        .join("/");
                    if let Some(glob) = GlobBuilder::new(&item)
                        .literal_separator(true)
                        .case_insensitive(cfg!(windows))
                        .build()
                        .log_err()
                    {
                        globs.add(glob);
                        glob_directories.push(directory);
                    }
                } else {
                    add_with_ancestors(&mut paths, &item);
                }
            }
            ProjectItems {
                paths,
                globs: globs.build().log_err().unwrap_or_else(GlobSet::empty),
                glob_directories,
            }
        });

        Self {
            directory,
            output_directories,
            items,
        }
    }

    fn contains(&self, path: &str, is_dir: bool) -> bool {
        if path.is_empty() {
            return true;
        }
        if self
            .output_directories
            .iter()
            .any(|output_directory| is_within(path, output_directory))
        {
            return false;
        }
        match &self.items {
            Some(items) => {
                items.paths.contains(path)
                    || if is_dir {
                        items.glob_directories.iter().any(|directory| {
                            is_within(path, directory) || is_within(directory, path)
                        })
                    } else {
                        items.globs.is_match(path)
                    }
            }
            // These mirror the SDK's default item excludes.
            None => {
                !path.split('/').any(|component| component.starts_with('.'))
                    && !path.ends_with(".user")
            }
        }
    }
}

/// Normalizes a path for lookups. Paths are case-insensitive on Windows.
fn path_key(path: &str) -> String {
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_owned()
    }
}

/// Converts a path from a project file, relative to the project directory, into a lookup key.
/// Paths that leave the project directory aren't shown under it, so they are skipped.
fn project_relative_path(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    let path = path.trim_start_matches("./").trim_end_matches('/');
    if path.is_empty() || path.starts_with('/') || path.contains(':') {
        return None;
    }
    if path.split('/').any(|component| component == "..") {
        return None;
    }
    Some(path_key(path))
}

fn worktree_relative_path(
    worktrees: &[(WorktreeId, Arc<Path>)],
    path: &Path,
) -> Option<(WorktreeId, String)> {
    worktrees.iter().find_map(|(worktree_id, worktree_path)| {
        let relative_path = path.strip_prefix(worktree_path).ok()?;
        let components = relative_path
            .components()
            .map(|component| match component {
                Component::Normal(name) => Some(name.to_string_lossy()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        Some((*worktree_id, path_key(&components.join("/"))))
    })
}

fn add_with_ancestors(paths: &mut HashSet<String>, path: &str) {
    let mut ancestor = path;
    while !ancestor.is_empty() && paths.insert(ancestor.to_owned()) {
        ancestor = ancestor.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
}

fn is_within(path: &str, directory: &str) -> bool {
    directory.is_empty()
        || path == directory
        || path
            .strip_prefix(directory)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn relative_to<'a>(path: &'a str, directory: &str) -> &'a str {
    if directory.is_empty() {
        path
    } else {
        path.strip_prefix(directory)
            .map_or(path, |rest| rest.trim_start_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msbuild::{parse_project, parse_solution};
    use indoc::indoc;
    use std::path::PathBuf;

    #[test]
    fn includes_solution_and_project_files() {
        let root = PathBuf::from(if cfg!(windows) { "C:\\work" } else { "/work" });
        let worktree_id = WorktreeId::from_usize(1);
        let solution = parse_solution(
            &root.join("App.sln"),
            indoc! {r#"
                Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Api", "src\Api\Api.csproj", "{4CFE6531-67B4-4C56-9DEE-79F74621AE96}"
                EndProject
                Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "Solution Items", "Solution Items", "{8C1E2D3F-4A5B-6C7D-8E9F-0A1B2C3D4E5F}"
                    ProjectSection(SolutionItems) = preProject
                        Directory.Build.props = Directory.Build.props
                    EndProjectSection
                EndProject
            "#},
        )
        .unwrap();
        let sdk_project = parse_project(
            &root.join("src").join("Api").join("Api.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk.Web"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>"#,
        )
        .unwrap();
        let legacy_project = parse_project(
            &root.join("Legacy").join("Legacy.csproj"),
            indoc! {r#"
                <Project ToolsVersion="15.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
                  <ItemGroup>
                    <Compile Include="Global.asax.cs" />
                    <Content Include="Content\Site.css" />
                    <Content Include="Scripts\**\*.js" />
                  </ItemGroup>
                </Project>
            "#},
        )
        .unwrap();
        let view = SolutionView::new(
            &[(worktree_id, Arc::from(root.as_path()))],
            &[solution],
            &[sdk_project, legacy_project],
        )
        .unwrap();

        let contains = |path: &str, is_dir: bool| {
            view.contains(worktree_id, RelPath::from_unix_str(path).unwrap(), is_dir)
        };
        let visible = [
            ("App.sln", false),
            ("Directory.Build.props", false),
            ("src", true),
            ("src/Api", true),
            ("src/Api/Api.csproj", false),
            ("src/Api/Program.cs", false),
            ("src/Api/Controllers", true),
            ("src/Api/Controllers/HomeController.cs", false),
            ("Legacy", true),
            ("Legacy/Legacy.csproj", false),
            ("Legacy/Global.asax.cs", false),
            ("Legacy/Content", true),
            ("Legacy/Content/Site.css", false),
            ("Legacy/Scripts", true),
            ("Legacy/Scripts/lib", true),
            ("Legacy/Scripts/lib/jquery.js", false),
        ];
        let hidden = [
            ("README.md", false),
            ("packages", true),
            ("docs", true),
            ("src/Api/bin", true),
            ("src/Api/obj/project.assets.json", false),
            ("src/Api/.vs", true),
            ("src/Api/Api.csproj.user", false),
            ("Legacy/Unlisted.cs", false),
            ("Legacy/Scripts/readme.txt", false),
            ("Legacy/bin", true),
            ("Legacy/Content/Images", true),
        ];
        for (path, is_dir) in visible {
            assert!(
                contains(path, is_dir),
                "{path} should be in the solution view"
            );
        }
        for (path, is_dir) in hidden {
            assert!(
                !contains(path, is_dir),
                "{path} should not be in the solution view"
            );
        }

        assert!(
            view.contains(
                WorktreeId::from_usize(2),
                RelPath::from_unix_str("README.md").unwrap(),
                false
            ),
            "worktrees without .NET projects are shown in full"
        );
    }
}
