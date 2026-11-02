# dotnet_ui

A Visual Studio-style Solution Explorer for .NET workspaces, built on the project model in [`dotnet`](../dotnet), which discovers `.sln`, `.slnx` and project files in local worktrees and keeps them up to date as files change.

## Solution Explorer

`solution explorer: toggle focus`, or the blocks icon in the dock's status bar (it only shows when the workspace has .NET projects).

- Solutions (`.sln` and `.slnx`), each with its projects; projects no solution lists appear under "Projects without a solution".
- Per project: target frameworks, whether it's a legacy (non-SDK) project, and whether it's the startup project; its **Projects** (project references) and **Packages** (`PackageReference` items, or `packages.config` for legacy projects; "central" marks versions from central package management).
- The toolbar shows the startup project (the selected run configuration) with Run, Debug and Build Solution buttons.
- Right-click a solution: Build, Rebuild or Clean the solution, open the solution file.
- Right-click a project: Build, Rebuild, Clean, Run Tests (test projects), Run or Debug each run configuration (launch profile and target framework), Set as Startup Project, open the project file.
- Double-click a solution or project to open its file; click a project reference to open it.

Commands run as Zed tasks in the terminal panel, using `dotnet build/run/test` for SDK-style projects and Visual Studio's MSBuild.exe for legacy ones (see the `dotnet` crate). Debugging uses the `netcoredbg` adapter from the C# extension ([`extensions/csharp`](../../extensions/csharp)), so it needs that extension installed.

## Testing

```sh
cargo test -p dotnet
./script/clippy -p dotnet -p dotnet_ui
```

To try it, run Zed (`cargo run`), open a folder with a solution, for example one created with:

```sh
dotnet new sln -n Sample
dotnet new webapi -o src/Api
dotnet new xunit -o tests/Api.Tests
dotnet sln add src/Api tests/Api.Tests
dotnet add tests/Api.Tests reference src/Api
```

and check that the Solution Explorer shows both projects with their references and packages, and that the context menu's Build/Run/Run Tests start terminal tasks.
