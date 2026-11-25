# Fork roadmap: Zed instead of WebStorm, Rider and DataGrip

Goal: do all daily work for React (TS/TSX) front ends, .NET Framework and .NET Core back ends,
and SQL Server / Azure SQL databases in this fork. Visual Studio stays installed as a fallback
for the things listed under [Left to Visual Studio](#left-to-visual-studio). Nothing else should need it.

Ground rules (from earlier sessions):

- The fork tracks `zed-industries/zed`. New features go in new crates and extensions
  (`dotnet`, `dotnet_ui`, `database`, `database_ui`, `extensions/*`). Keep edits to upstream
  crates small and in narrow hooks.
- No native UI rewrites.
- The debugger must be one Zed may ship: vsdbg is licensed only for Microsoft tools. GPL or MIT is fine.

Sizes: **S** is a day or less, **M** is a few days, **L** is a week or more.

## Where things stand (updated 2026-11-13)

Done since the first draft: 1.1 .NET Framework debugging; 2.1 SQL completion; 2.2 cancel,
paging and NULL cells; the Web Forms and Web.config languages. Database consoles also run
`GO`-separated scripts batch by batch, keep their session state between runs, and push into an
open transaction through a savepoint.

| Area | In place | Biggest gaps |
| --- | --- | --- |
| .NET editing | Roslyn LSP; C#, MSBuild and XML grammars; gutter runnables for xUnit, NUnit and MSTest | Razor (`.cshtml`/`.razor`); Roslyn code lens commands |
| .NET projects | `.sln`/`.slnx`/`.csproj` discovery; SDK-style and legacy parsing; Solution view; Solution Explorer; Open Solution | No file nodes in Solution Explorer; adding or renaming a file doesn't update a legacy csproj |
| .NET build and run | `dotnet build`/`run`; MSBuild.exe through vswhere for legacy projects; IIS Express for System.Web projects; launch profiles × TFMs | No compound configs; IIS Express profiles in `launchSettings.json` ignored |
| .NET debugging | netcoredbg for .NET Core; dotnet-debugger (MIT, ICorDebug) for net4x, including legacy exes and IIS Express sites; attach picker; `dotnet: attach to IIS Express` | 64-bit processes only (32-bit IIS Express is refused); can't debug tests |
| .NET tests | Gutter "run test" through `dotnet test --filter` | **No test explorer**; legacy (vstest) test projects can't run |
| NuGet | Browse, install, update and remove against nuget.org | No `nuget.config` or private feeds; `packages.config` read-only |
| Web Forms | `.aspx`/`.ascx`/`.master` highlighting with C# injection; Web.config language | No code-behind navigation; no completion |
| Databases | SQL Server and Postgres; LocalDB; environments with prod coloring; schema tree; consoles with lasting session state and `GO` batches; schema-aware SQL completion; Stop, timeouts, 500-row paging, NULL cells; editable grid with reviewed, transactional push; export of a run's results to Markdown, JSON, CSV or TSV | No routines or DDL, no history, no range copy, no Entra ID; Postgres push still commits an open transaction block |
| Front end | Upstream Zed: vtsls, ESLint, Prettier, js-debug (Chrome/Node), package.json script tasks, Jest and Vitest runnables | No test explorer; no HTTP client; no compound "API + Vite" run |
| VCS | Git (upstream); Subversion working copies (status, diff, blame, commit, update, log) | SVN branches/switch, merge, properties, file-history changes |
| General | JetBrains base keymap; outline, call hierarchy, bookmarks (upstream) | No local history; no JetBrains-style bindings for fork actions |

## Milestone 1: Rider can go

These are the items that send work back to Rider or Visual Studio every week.

### 1.1 .NET Framework debugging (L) — must-have — **done 2026-11-13**

Shipped with dotnet-debugger (from nuget.org). The C# extension uses it for net4x, legacy exes and
64-bit IIS Express, and netcoredbg for .NET Core. Attach works from the picker and from
`dotnet: attach to IIS Express`. Still open: 32-bit processes, which would need a 32-bit build of
the adapter, and debugging tests (see 1.2). The original plan follows for reference.

1. **Pick an adapter.**
   - First, evaluate [`dotnet-debugger-dap`](https://www.nuget.org/packages/dotnet-debugger-dap) (MIT, C#, ICorDebug).
     - Its NuGet page claims .NET Framework 4.x on Windows: launch and attach, MSBuild.exe-built legacy projects, Windows PDBs and several AppDomains.
     - It is **64-bit only**. Repository: `github.com/dvislobokov/dotnet-debugger`.
   - SharpDbg (`MattParkerDev/sharpdbg`, also forked as `lextudio/sharpdbg`) is a second candidate. It is a ClrDebug-based DAP, but its README targets .NET 5+.
   - If neither holds up, build our own adapter. The 2026-10-29 spike proved that ClrDebug and dnlib can debug net48 with both portable and Windows PDBs.
2. **Wire the adapter into `extensions/csharp`** next to netcoredbg.
   - The locator picks the adapter by TFM: netcoredbg for .NET Core, the Framework adapter for net4x.
   - Then flip `can_debug()`.
3. **Web Forms / MVC 5 under IIS Express.**
   - Start `iisexpress.exe` (the existing code path), then attach to its PID.
   - IIS Express runs **32-bit by default**. With a 64-bit-only adapter, we must launch `C:\Program Files\IIS Express\iisexpress.exe` (x64). Projects that set `Use64BitIISExpress=false`, or depend on 32-bit COM, need a 32-bit build of the adapter.
4. **Attach to process.**
   - Expose the adapter's `attach` through upstream's attach modal (`crates/debugger_ui/src/attach_modal.rs`), filtered to `w3wp.exe`, `iisexpress.exe`, `dotnet.exe` and the startup project's exe.
   - Add a "Debug: Attach to IIS Express" action.
5. Acceptance: breakpoints, stepping, locals, watch, call stack and break-on-exception work on a net48 console app and on a Web Forms site, using both PDB formats.

Open question: are any of the Framework apps 32-bit only (x86 platform target, 32-bit COM, old Oracle or Jet drivers)?

### 1.2 Test explorer (L) — must-have

There is a run-test runnable, but no tree, no results view, and no way to debug a test.

- **New crate `test_explorer`** with a `TestProvider` trait (discover, run, debug, cancel) and a dock panel.
  - Tree: project → namespace → class → test.
  - Pass/fail/skip icons, duration, output and stack trace for the selected test.
  - "Rerun failed" and "Run all in solution".
  - Gutter icons in the editor show the last result.
- **.NET provider, SDK-style projects:**
  - Discover with `dotnet test --list-tests`, or build and run with `--logger trx` to get fully qualified names and source locations.
  - Run with a `--filter` and parse TRX.
  - Debug by running `dotnet test` with `VSTEST_HOST_DEBUG=1`, reading the testhost PID and attaching. Use netcoredbg or the 1.1 adapter, depending on the TFM.
- **.NET provider, legacy projects:**
  - Find `vstest.console.exe` with vswhere, then use `/ListFullyQualifiedTests` and `/TestCaseFilter` with `/logger:trx`.
  - This fixes "run its tests with vstest.console.exe" (`crates/dotnet/src/dotnet.rs:260`).
- **JS providers:** Vitest (`--reporter=json`) and Jest (`--json --outputFile`). Debug through js-debug. These can land in Milestone 3.

### 1.3 Keep legacy csproj files in sync (M)

In non-SDK projects, a file only compiles if the csproj lists it. Today, adding, renaming or deleting a `.cs` file in Zed silently drops it from the build.

- Watch worktree create, rename and delete events under a legacy project's folder. Update `<Compile Include>`, `<Content Include>` and `<None Include>` items.
  - Keep `DependentUpon` for `.aspx.cs` and `.designer.cs` files.
  - Preserve the csproj's formatting with a minimal text edit.
- Add "Include in project" and "Exclude from project" to the Solution view context menu.

### 1.4 Solution Explorer file nodes and templates (M)

- Show files under each project, with nesting such as `Foo.aspx` → `.aspx.cs`/`.designer.cs` and `appsettings.*.json`. This turns the panel into Rider's Solution view instead of a list of projects.
- Add "New → Class / Interface / Enum / Razor page" from small templates, using namespaces derived from the folder. Also add "Add project reference", "Add existing project" and "New project" (`dotnet new` list).

### 1.5 NuGet private feeds and packages.config (M)

- Read the `nuget.config` chain (solution dir → user → machine) and query every v3 source.
- Authenticate through the Azure Artifacts Credential Provider when it's installed, or with stored credentials in the OS keychain (as database passwords are).
- `packages.config` projects: install or update through `nuget.exe`, found or downloaded once, then fix the csproj `HintPath`s. Or show the exact command if we decide not to edit these projects.

Open question: which private feeds are in use (Azure Artifacts, GitHub Packages, a file share)?

### 1.6 JetBrains bindings for fork actions (S)

With `base_keymap: JetBrains`, bind:

- Build: `ctrl-f9`
- Run and debug the selected configuration: `shift-f10` and `shift-f9`
- Run the test at the cursor: `ctrl-shift-f10`
- Solution Explorer: `alt-1`, next to the project panel
- Test explorer and Database panel: their own keys
- Run statement: `ctrl-enter`, as in DataGrip

Put these in the fork crates' default keymap fragments, not in upstream keymap files, to avoid merge conflicts.

## Milestone 2: DataGrip can go

### 2.1 SQL completion (L) — the main DataGrip gap — **done 2026-11-13**

Shipped: tables, typed columns, alias resolution, whole joins from foreign keys, and keywords,
from a per-database catalog cache. Still open: routines in completion, and go to definition (needs 2.3).

- Give database consoles (and `.sql` files bound with "Use for Current Editor") a completion provider that draws on cached catalog metadata:
  - schemas, tables, views, columns (with types), routines and keywords
  - alias resolution (`FROM dbo.Orders o` → `o.`)
  - `JOIN … ON` suggestions from foreign keys
- Load metadata lazily per database, and refresh it on demand or after DDL runs.
- Go to definition on a table or routine name opens its DDL (2.3).
- Prefer this over shelling out to a generic SQL LSP (`sqls`), because the panel already holds live connections and metadata.

### 2.2 Query cancel, row limits and streaming (M) — safety — **done 2026-11-13**

Shipped: Stop (`ctrl-f2`), per-connection timeouts, a 500-row fetch limit with Next 500 and All,
streaming rows, and a `<null>` cell type. On SQL Server, Stop keeps the session, including on TLS,
thanks to the vendored tiberius patch. The original plan follows.

- Add a Stop button and action. Use an attention/cancel request on SQL Server (tiberius) and `CancelToken` on Postgres.
- Set a default fetch limit (for example 500 rows) with "fetch next page" and "fetch all", streaming rows into the grid.
- Add per-connection timeouts.
- Introduce a real NULL cell type (the TODO in `crates/tabular_data_preview/src/renderer/render_table.rs`), so NULL differs from `''`.

### 2.3 Routines, keys, indexes and DDL (M)

- Add stored procedures, functions, triggers, indexes, keys and sequences to the tree.
- Add "Script as CREATE/ALTER" and "Open definition" (SQL Server `OBJECT_DEFINITION` / `sys.sql_modules`; Postgres `pg_get_functiondef`, `pg_get_viewdef`), plus table DDL generation.
- Add "Execute procedure…" that prompts for parameters.

### 2.4 Query history and export (M)

- History: keep every executed statement per connection (text, time, duration, row count, error) in a local SQLite store. Add a searchable picker and "re-open in console".
- Export: done for whole runs. **Export…** in the results header saves every result set as Markdown or JSON, or the open one as CSV or TSV. It fetches a paused run's remaining rows first.
- Still to do: export or copy a range selection (TSV pastes into Excel), and SQL `INSERT` output. Upstream just added range selection to the grid.

### 2.5 Transactions and prod guard rails (M)

- Add an auto-commit / manual toggle per console, Commit and Rollback buttons, and an "open transaction" indicator in the console tab. This builds on the per-console session that already exists.
- Production environments:
  - an optional read-only mode
  - confirmation before `UPDATE`/`DELETE` without `WHERE`, `DROP` or `TRUNCATE`
  - a tinted console background

### 2.6 Microsoft Entra ID auth for Azure SQL (M)

- tiberius takes an AAD access token. Get one from `az account get-access-token` or the Azure Identity flow (interactive browser or device code), and cache and refresh it.
- Open question: does the org enforce Entra-only auth on Azure SQL? If so, this moves up to Milestone 2's first item.

### 2.7 Grid editing completeness (M)

- Insert and delete rows, set NULL or default, and revert a cell.
- Open a table as an editable grid with WHERE and ORDER BY filters pushed down to the server.

Later (P2): explain plans (`SHOWPLAN_XML`, Postgres `EXPLAIN (FORMAT JSON)`) as a tree; SQL formatting (a `sql-formatter` extension or a formatter task); CSV import; MySQL or SQLite engines only if they turn up.

## Milestone 3: WebStorm can go

### 3.1 Test explorer JS providers (M)

Add Vitest and Jest to the 1.2 panel, with watch mode ("rerun on save") and debugging through js-debug.

### 3.2 HTTP client for `.http` files (M)

Use the format JetBrains and Visual Studio share, so existing request files work:

- `###` separators, `{{variables}}`, and `http-client.env.json` / `http-client.private.env.json` environments.
- A run gutter on each request.
- Show the response (status, headers, pretty-printed body, timing) in a read-only buffer.
- Keep a history of recent responses.

This needs a small new crate on top of Zed's `http_client`, plus a tree-sitter `http` grammar (the `zed-http` extension or `tree-sitter-http`).

### 3.3 Compound run configurations (M)

Run "API under debugger + `npm run dev`" with one key, like Rider's compound configurations:

- A compound entry in the dotnet run configuration picker that starts several dotnet configs, tasks and debug sessions together and stops them together.
- Upstream `debug.json` and `tasks.json` have no compound launch, so this lives in `dotnet_ui`.

### 3.4 Local history (M)

JetBrains' safety net:

- Save a snapshot of a file on each save and on external change, in a local store with retention by age or size.
- Add "Local History: Show for file", which lists revisions with diffs and supports revert.
- Snapshot whole folders before risky operations such as a VCS update or revert.

It works for both Git and SVN working copies. This is an upstream-shaped feature: build it as a separate crate.

Later (P2): an npm panel (package.json scripts already appear as tasks, so this is low value); inline hints for outdated `package.json` versions; coverage gutters from lcov (Vitest/Jest) and coverlet (`dotnet test --collect "XPlat Code Coverage"`).

## Milestone 4: Polish and the long tail

| Item | Size | Notes |
| --- | --- | --- |
| Web Forms: code-behind navigation | S | `.aspx` ↔ `.aspx.cs` ↔ `.designer.cs` swap action; `runat=server` ids → designer fields |
| Razor `.cshtml`/`.razor` | M | Roslyn LSP now hosts Razor in-process (dotnet/roslyn#83575, May 2026). Add a Razor language with tree-sitter-razor to the C# extension and route it to Roslyn. MVC 5 (.NET Framework) Razor may get weaker support than ASP.NET Core. |
| Roslyn code lens commands | M | Upstream Zed renders code lenses (`crates/editor/src/code_lens.rs`). Implement the client-side commands (references peek, run or debug test) so `roslyn.rs:130` can stop disabling them. |
| Decompiled and source-generated navigation | M | Go to definition into BCL and NuGet code (Roslyn decompiles to temp files), and `roslyn-source-generated://` documents (custom request `sourceGeneratedDocument/_roslyn_getText`). |
| Build errors in a problems list | S | Parse MSBuild/`dotnet build` output (`file(line,col): error CS1234`) into diagnostics, so build errors show in project diagnostics like Rider's Build tool window. |
| `launchSettings.json` IISExpress profiles | S | Used for SDK-style projects that run under IIS Express. |
| User secrets | S | "Manage User Secrets" opens `%APPDATA%\Microsoft\UserSecrets\<UserSecretsId>\secrets.json` (and creates the id). |
| SVN gaps | M | Switch or branch, merge, properties, file-history changed files. Ask which of these get daily use. |
| Hot reload | S | `dotnet watch` is a task today. Add a "Run with Hot Reload" configuration and a status item. |
| Web.config transforms | S | Preview the transformed `Web.Release.config` (MSBuild `TransformXml` task). |
| README cleanup | S | `README.md:2` still has the PR-template line "Remove this line to confirm…", added by ca0cd3faf4. |
| Fork release build | S | A script that builds `target\release`, copies it to a stable install folder, and keeps the user's daily Zed out of `target\`. |

## Left to Visual Studio

These are accepted fallbacks, rarely needed and expensive to build:

- WinForms, WPF and Web Forms visual designers, and the `.resx` designer
- Edit and Continue on .NET Framework, IntelliTrace, Historical Debugging
- Performance and memory profilers (use `dotnet-trace`, `dotnet-counters` or PerfView when possible)
- EF6 `.edmx` designer; WCF "Add Service Reference" (`dotnet-svcutil` covers SDK-style projects)
- SSDT `.sqlproj` schema compare and publish, SSIS and SSRS
- Web Deploy publish profiles (a `msbuild /p:PublishProfile=…` task covers most of it)

## Keeping the fork mergeable

- The 2026-11-11 sync (99 upstream commits) had three conflicts and one API break:
  - `Cargo.lock`
  - `tabular_data_preview` (upstream added range selection in the same grid code the fork extended)
  - `Workspace::prompt_for_open_path`, which gained a mode argument
- Upstream is actively building `tabular_data_preview`. Consider upstreaming the fork's small hooks (cell decorations, inline cell editor slot, `EditRequested`, `set_selection_enabled`) as a PR, so future syncs stop conflicting there.
- The 2026-11-13 sync (29 upstream commits) conflicted only in `Cargo.lock`.
- tiberius 0.13 is vendored in `tooling/patches/tiberius` through `[patch.crates-io]`, so that
  Stop keeps the session on TLS connections (see its `PATCHES.md`). Drop the patch once upstream
  tiberius reads the separate Attention acknowledgement.
- Sync often: small merges are cheaper than 100-commit ones.

## Suggested order

Done: 1.1 .NET Framework debugging, 2.2 cancel/limits, 2.1 SQL completion.

1. **1.2 test explorer**, .NET first (including debugging tests), then 3.1.
2. **1.3 csproj sync** and **1.5 NuGet feeds**: small but painful in legacy projects.
3. **2.3 routines and DDL**, **2.4 history and export**, and **2.5 transactions UI**.
4. Then Milestone 3, in a session of its own, since it touches separate crates.
