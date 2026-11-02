//! The Solution Explorer and NuGet package manager panels for .NET workspaces, built on the
//! project model in the `dotnet` crate.

mod nuget;
mod nuget_panel;
mod solution_explorer;

pub use nuget_panel::NuGetPanel;
pub use solution_explorer::SolutionExplorer;
