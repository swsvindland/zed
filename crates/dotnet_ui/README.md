# dotnet_ui

Visual Studio-style panels for .NET workspaces, built on the project model in [`dotnet`](../dotnet), which discovers `.sln`, `.slnx` and project files in local worktrees and keeps them up to date as files change.

## Solution Explorer

`solution explorer: toggle focus`, or the blocks icon in the dock's status bar (it only shows when the workspace has .NET projects).

- Solutions (`.sln` and `.slnx`), each with its projects; projects no solution lists appear under "Projects without a solution".
- Per project: target frameworks, whether it's a legacy (non-SDK) project, and whether it's the startup project; its **Projects** (project references) and **Packages** (`PackageReference` items, or `packages.config` for legacy projects; "central" marks versions from central package management).
- The toolbar shows the startup project (the selected run configuration) with Run, Debug and Build Solution buttons.
- Right-click a solution: Build, Rebuild or Clean the solution, open the solution file.
- Right-click a project: Build, Rebuild, Clean, Run Tests (test projects), Run or Debug each run configuration (launch profile and target framework), Set as Startup Project, Manage NuGet Packages…, open the project file.
- Double-click a solution or project to open its file; click a project reference to open it; click a package to manage it in the NuGet panel.

Commands run as Zed tasks in the terminal panel, using `dotnet build/run/test` for SDK-style projects and Visual Studio's MSBuild.exe for legacy ones (see the `dotnet` crate). Debugging uses the `netcoredbg` adapter from the C# extension ([`extensions/csharp`](../../extensions/csharp)), so it needs that extension installed.

## NuGet

`nuget: toggle focus`, the box icon in the status bar, or "Manage NuGet Packages…" on a project.

- Pick the project at the top. **Browse** searches nuget.org (v3 search API, optionally including prereleases) and marks installed packages and available updates; **Installed** lists the project's packages, filtered by the search text.
- Select a package to see its description and versions (from the v3 package base address), choose a version, then **Install**/**Update** or **Remove**.
- Changes run `dotnet add <project> package <id> --version <version>` or `dotnet remove <project> package <id>` in the background; the panel and Solution Explorer refresh when the project file changes.

### packages.config projects

`dotnet add/remove package` only edits `PackageReference` items, so the panel shows the packages of `packages.config` projects read-only. Changing them would need what Visual Studio's NuGet does: download the package into the solution's `packages` folder (`nuget.exe install <id> -Version <version> -OutputDirectory packages`), update `packages.config`, add `<Reference>` items with `HintPath`s for each assembly the package has for the project's framework, and add any `.props`/`.targets` imports and content files. Alternatively, migrate the project to `PackageReference` (Visual Studio's "Migrate packages.config to PackageReference").

## Testing

```sh
cargo test -p dotnet -p dotnet_ui
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

and check that the Solution Explorer shows both projects with their references and packages, that the context menu's Build/Run/Run Tests start terminal tasks, and that installing a package from the NuGet panel adds it to the project file and the tree.
