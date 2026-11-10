# Web.config extension

Highlighting, indentation, brackets and an outline for .NET configuration files, using [tree-sitter-xml](https://github.com/tree-sitter-grammars/tree-sitter-xml).

It claims `Web.config`, `App.config` and their `Debug`/`Release` transforms by full file name, so it takes priority over any XML language that matches every `.config` file. For other transforms (for example `Web.Staging.config`), add them in settings:

```json
{ "file_types": { "Web.config": ["Web.*.config"] } }
```

The outline lists the sections under `<configuration>`, each `<location path="…">`, and every element with a `key` or `name` attribute (app settings, connection strings, config sections, binding redirects, rewrite rules and handlers).

## Install

Run `zed: install dev extension` and pick `extensions/webconfig`. Query errors show up in the log (`zed: open log`) when the first matching file opens.
