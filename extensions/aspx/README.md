# ASP.NET Web Forms extension

Highlighting for `.aspx`, `.ascx`, `.master` and `.asax` files.

There is no tree-sitter grammar for Web Forms, so this uses [tree-sitter-embedded-template](https://github.com/tree-sitter/tree-sitter-embedded-template) (the ERB/EJS grammar) for the server tags, and injects:

| Construct | Language |
| --- | --- |
| HTML and server controls (`<asp:Button runat="server" />`) | `ASPX Markup`, a hidden copy of HTML |
| `<% statements %>` | C#, all blocks combined into one program |
| `<%= value %>`, `<%: value %>`, `<%# Eval("Name") %>`, `<%#: Item.Name %>` | C#, each block on its own |
| `<script runat="server">` | C# |
| `<script>`, `onclick`, `OnClientClick` | JavaScript |
| `<style>`, `style` | CSS |

`<%@ Page … %>` directives and `<%$ … %>` expression builders are highlighted as a whole, and `<%-- … --%>` as a comment. C# highlighting needs a C# extension (the `csharp-dotnet` dev extension or the registry's `csharp`); JavaScript and CSS need their usual languages.

Limits of reusing the ERB grammar:

- Directive attributes aren't highlighted one by one.
- A server comment ends at the first `%>`, so commenting out markup that contains `<% … %>` ends the comment early.
- `<%:` and `<%#:` pass their leading `:` to the C# parser, which skips it as an error.

`<script runat="server">` is an injection two levels deep (ASPX → ASPX Markup → C#). Zed builds without this fork's syntax map fix (`crates/language`) only highlight it after the first edit in it, when the page opens before the C# grammar has loaded.

## Install

Run `zed: install dev extension` and pick `extensions/aspx`. Zed clones and compiles both grammars; query errors show up in the log (`zed: open log`) when the first ASPX file opens.
