# C# (.NET) extension

C# support for this Zed fork:

- **C#** (`.cs`, `.csx`) with the [tree-sitter-c-sharp](https://github.com/tree-sitter/tree-sitter-c-sharp) grammar: highlighting, indentation, brackets, outline, text objects and runnables.
- **MSBuild** (`.csproj`, `.fsproj`, `.vbproj`, `.proj`, `.props`, `.targets`, `.slnx`) and **XML** (`.xml`, `.config`, `.resx`, `.nuspec`, `.xaml`, …) with [tree-sitter-xml](https://github.com/tree-sitter-grammars/tree-sitter-xml).
- **Roslyn** (`Microsoft.CodeAnalysis.LanguageServer`, MIT) as the language server. The extension downloads the `roslyn-language-server.<rid>` package from nuget.org into its work directory and starts it with `--autoLoadProjects`, so it finds the solution or projects in the worktree by itself.
- **netcoredbg** (MIT, Samsung) as the `netcoredbg` debug adapter for .NET (Core) programs, downloaded from its GitHub releases.

Run and debug of projects (`dotnet: run`, `dotnet: debug`, the run configuration in the status bar, and the `dotnet run` → netcoredbg locator) live in the native [`dotnet`](../../crates/dotnet) crate, because the extension sandbox can only read the extension's own directory and so can't read `launchSettings.json` or find a file's project.

Queries and the NuGet download code are adapted from [zed-extensions/csharp](https://github.com/zed-extensions/csharp) (Apache-2.0).

## Install

Requires the `wasm32-wasip2` Rust target (`rustup target add wasm32-wasip2`) and the .NET SDK on `PATH`.

1. Run Zed from this repository (`cargo run`).
2. Run `zed: install dev extension` and pick `extensions/csharp`.
3. Open a folder that contains a `.sln`, `.slnx` or project. The first C# file starts Roslyn; the download (about 50 MB) shows in the activity indicator.

## Tasks and runnables

The C# language contributes these tasks (`task: spawn`), and play buttons in the gutter for tests, `Main` and the first top-level statement:

| Task | Runs in |
| --- | --- |
| `dotnet run` | the file's directory, so it suits `Program.cs` at the project root |
| `dotnet test --filter <Class>.<Method>` / `<Class>` | the worktree root (the solution) |
| `dotnet build (workspace)`, `dotnet test (workspace)` | the worktree root |
| `dotnet watch run` | the file's directory |

MSBuild files contribute `dotnet build/run/test/restore/clean` for the open project file.

For anything that needs the right project from any file, use the `dotnet:` actions from the `dotnet` crate instead.

## Debugging

- `dotnet: debug` (or the bug button next to the run configuration in the status bar) builds the selected project and launch profile and starts it under netcoredbg.
- In `debugger: start`, every `dotnet run` task (for example the gutter's `dotnet run`, or the MSBuild file's `dotnet run --project`) can also be debugged: the `dotnet-locator` turns it into a `dotnet build` followed by a netcoredbg launch.
- `.zed/debug.json` scenarios work too:

  ```json
  [
    {
      "label": "Debug Api",
      "adapter": "netcoredbg",
      "request": "launch",
      "program": "$ZED_WORKTREE_ROOT/src/Api/bin/Debug/net10.0/Api.dll",
      "cwd": "$ZED_WORKTREE_ROOT/src/Api",
      "env": { "ASPNETCORE_ENVIRONMENT": "Development" }
    }
  ]
  ```

netcoredbg can't debug .NET Framework programs; that needs a separate ICorDebug-based adapter.

## Settings

Roslyn options use their VS Code names, either flat or grouped by `language|category`:

```json
{
  "lsp": {
    "roslyn": {
      "settings": {
        "csharp|inlay_hints": { "csharp_enable_inlay_hints_for_types": false },
        "csharp|background_analysis.dotnet_analyzer_diagnostics_scope": "fullSolution"
      },
      "binary": { "path": "C:/tools/roslyn/Microsoft.CodeAnalysis.LanguageServer.exe" }
    }
  },
  "debugger": { "dap": { "netcoredbg": { "binary": "C:/tools/netcoredbg/netcoredbg.exe" } } }
}
```

A `binary.path` replaces the downloaded server; `binary.arguments` replaces the default `--stdio --autoLoadProjects --logLevel=Information`. A `netcoredbg` on `PATH` is used before downloading one.

## Testing

```sh
cargo build -p zed_csharp --target wasm32-wasip2
./script/clippy -p zed_csharp
```

Then install it as a dev extension (above) and check, on a sample such as `dotnet new webapi`:

- highlighting and the outline (`outline: toggle`) in `Program.cs`;
- completions and diagnostics from Roslyn (introduce a type error);
- a breakpoint hit through `dotnet: debug`;
- the gutter test buttons in an xUnit project.

Razor (`.razor`, `.cshtml`) language support is not included yet. Roslyn serves Razor through Razor's "cohosting" assemblies (`Microsoft.VisualStudioCode.RazorExtension`), which ship inside VS Code C# extension releases; nuget.org only has one 2025 preview of them, so they would have to be downloaded in a version that matches the Roslyn build.
