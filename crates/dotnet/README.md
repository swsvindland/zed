# dotnet

The .NET project model and run/build/debug commands for this Zed fork. It lives in core because extensions can't read project files outside their sandbox or contribute pickers, status bar items and panels.

- **Discovery**: finds `.sln`, `.slnx`, `.csproj`, `.vbproj` and `.fsproj` files in local worktrees and parses them (`msbuild.rs`): SDK-style vs. legacy projects, output type, target frameworks, `launchSettings.json` profiles, project references, package references (`PackageReference` or `packages.config`) and test projects. It rescans when those files change.
- **Run configurations**: one per runnable project, launch profile and target framework, shown in the status bar. `dotnet: select run configuration` picks one (the "startup project"), `dotnet: run` runs it.
- **Builds**: `dotnet: build`, `dotnet: build solution`, `rebuild solution`, `clean solution`. SDK-style projects use `dotnet`; legacy .NET Framework projects use Visual Studio's MSBuild.exe (found with `vswhere`), restoring `packages.config`, and ASP.NET (System.Web) projects run under IIS Express.
- **Debugging** (`debug.rs`): `dotnet: debug`, or the bug button in the status bar, builds the selected configuration and starts it under the `netcoredbg` adapter that the C# extension ([`extensions/csharp`](../../extensions/csharp)) provides. The `dotnet-locator` also turns any `dotnet run …` task into a debug scenario in `debugger: start`: it builds with `dotnet build`, asks MSBuild for the output assembly (`-getProperty:TargetPath`), and applies the launch profile `dotnet run` would use (`applicationUrl` as `ASPNETCORE_URLS`, environment variables, `commandLineArgs`, `workingDirectory`). .NET Framework programs can't be debugged yet.
- **Solution view**: a "Solution | Files" switch in the project panel that hides `bin`/`obj` and shows what each project contains.

[`dotnet_ui`](../dotnet_ui) adds the Solution Explorer and NuGet panels on top of this model.

A Visual Studio-style F5 is a keymap entry away:

```json
[{ "context": "Workspace", "bindings": { "f5": "dotnet::Debug", "ctrl-f5": "dotnet::Run", "ctrl-shift-b": "dotnet::BuildSolution" } }]
```

## Testing

```sh
cargo test -p dotnet
./script/clippy -p dotnet
```

Then run Zed, open a folder with an ASP.NET Core project and the C# extension installed, set a breakpoint in a request handler, run `dotnet: debug`, and request the endpoint.
