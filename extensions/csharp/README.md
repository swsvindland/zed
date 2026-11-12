# C# (.NET) extension

C# support for this Zed fork:

- **C#** (`.cs`, `.csx`) with the [tree-sitter-c-sharp](https://github.com/tree-sitter/tree-sitter-c-sharp) grammar: highlighting, indentation, brackets, outline, text objects and runnables.
- **MSBuild** (`.csproj`, `.fsproj`, `.vbproj`, `.proj`, `.props`, `.targets`, `.slnx`) and **XML** (`.xml`, `.config`, `.resx`, `.nuspec`, `.xaml`, …) with [tree-sitter-xml](https://github.com/tree-sitter-grammars/tree-sitter-xml).
- **Roslyn** (`Microsoft.CodeAnalysis.LanguageServer`, MIT) as the language server. The extension downloads the `roslyn-language-server.<rid>` package from nuget.org into its work directory and starts it with `--autoLoadProjects`, so it finds the solution or projects in the worktree by itself.
- **netcoredbg** (MIT, Samsung) as the `netcoredbg` debug adapter for .NET (Core) programs, downloaded from its GitHub releases.
- **dotnet-debugger** (MIT, [dvislobokov/dotnet-debugger](https://github.com/dvislobokov/dotnet-debugger)) as the `dotnet-debugger` debug adapter for .NET Framework 4.x programs. It's an ICorDebug-based adapter; the extension downloads its `dotnet-debugger-dap` tool package from nuget.org and runs it with `dotnet` (.NET 8 or later).

Run and debug of projects (`dotnet: run`, `dotnet: debug`, the run configuration in the status bar, and the `dotnet run` → debugger locator) live in the native [`dotnet`](../../crates/dotnet) crate, because the extension sandbox can only read the extension's own directory and so can't read `launchSettings.json` or find a file's project.

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

`dotnet: debug` (or the bug button next to the run configuration in the status bar) builds the selected run configuration and starts it under the debugger that fits its target framework:

| Run configuration | Build | Debugger |
| --- | --- | --- |
| SDK project, .NET (Core) | `dotnet build` | netcoredbg launches it |
| SDK project, `net4x` | `dotnet build` | dotnet-debugger launches it |
| Legacy console or desktop app | MSBuild.exe | dotnet-debugger launches it from its output directory |
| Legacy web project (Web Forms, MVC 5) | MSBuild.exe | 64-bit IIS Express starts in a terminal, then dotnet-debugger attaches to it |

- In `debugger: start`, every `dotnet run` task (for example the gutter's `dotnet run`, or the MSBuild file's `dotnet run --project`) can also be debugged: the `dotnet-locator` turns it into a `dotnet build` followed by a launch. The locator reads the project to pick dotnet-debugger for `net4x`; for a task whose folder is still a variable (the gutter's `$ZED_DIRNAME`), it keeps the C# language's first debugger, netcoredbg, and says so if the program turns out to be .NET Framework. Making dotnet-debugger the C# debugger (`"languages": { "C#": { "debuggers": ["dotnet-debugger"] } }`) uses it for .NET 8+ too.
- To attach to a running process, open `debugger: start`, choose the **Attach** tab, pick `dotnet-debugger` in the debugger dropdown at its bottom right (it starts on `netcoredbg`, which only attaches to .NET (Core) processes), and pick the process, such as `w3wp.exe`, `iisexpress.exe` or your app. A `.zed/debug.json` scenario with `"request": "attach", "processId": "$ZED_PICK_PID"` opens the same picker.
- Stopping a session ends the debugged process, attached or not; that's how stopping an IIS Express session stops the site. Use **Disconnect** in the debug panel to detach instead.
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

  For a .NET Framework program, use `"adapter": "dotnet-debugger"` and point `program` at its `.exe`. dotnet-debugger also takes `"project"` (with `"build": true`) instead of `program`, `"console": "integratedTerminal"` for a program that reads standard input, and `justMyCode`, `symbolOptions` and `sourceFileMap`; see [its schema](debug_adapter_schemas/dotnet-debugger.json).

### .NET Framework limits

dotnet-debugger debugs both Windows (full/pdbonly) and portable PDBs, several AppDomains (ASP.NET runs each site in its own), async code and exceptions. It only debugs **64-bit processes**:

- A console or desktop app built for `x86`, or for `AnyCPU` with `Prefer32Bit` (the default of older Visual Studio templates), is refused when it starts, with a message that says so. Set `<Prefer32Bit>false</Prefer32Bit>` in its Debug configuration, unless it really needs 32-bit (32-bit COM components, Jet or old Oracle drivers).
- Web projects are served by the 64-bit IIS Express, `C:\Program Files\IIS Express\iisexpress.exe`. `dotnet: debug` refuses projects that set `Use64BitIISExpress` to `false` or build for `x86`, and says why; `dotnet: run` still runs them.
- .NET Framework 3.5 and earlier (CLR 2) can't be debugged.

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
  "dap": {
    "netcoredbg": { "binary": "C:/tools/netcoredbg/netcoredbg.exe" },
    "dotnet-debugger": { "binary": "C:/src/dotnet-debugger/artifacts/publish/win-x64/dotnet-debugger.exe" }
  }
}
```

A `binary.path` replaces the downloaded server; `binary.arguments` replaces the default `--stdio --autoLoadProjects --logLevel=Information`. A `netcoredbg` on `PATH` is used before downloading one, and so is a `dotnet-debugger` on `PATH` (`dotnet tool install -g dotnet-debugger-dap`). A `dotnet-debugger` binary that ends in `.dll` is run with `dotnet`.

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

For .NET Framework debugging, use a legacy (non-SDK) `net48` console project and a Web Forms project, both built with MSBuild.exe: a breakpoint in `Main` and in `Page_Load` should stop through `dotnet: debug`, with locals, the call stack and stepping working. Build the console app with both `<DebugType>full</DebugType>` and `portable`.

Razor (`.razor`, `.cshtml`) language support is not included yet. Roslyn serves Razor through Razor's "cohosting" assemblies (`Microsoft.VisualStudioCode.RazorExtension`), which ship inside VS Code C# extension releases; nuget.org only has one 2025 preview of them, so they would have to be downloaded in a version that matches the Roslyn build.
