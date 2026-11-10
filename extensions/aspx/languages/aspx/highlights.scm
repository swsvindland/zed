[
  "<%"
  "<%="
  "<%#"
  "%>"
] @keyword

; <%@ Page Language="C#" CodeBehind="Default.aspx.cs" %>
((directive
  (code) @preproc)
  (#match? @preproc "^@"))

; <%$ AppSettings:Key %>, <%$ Resources:Strings, Title %>
((directive
  (code) @string.special)
  (#match? @string.special "^\\$"))

; The grammar reads a server comment `<%-- … --%>` as `<%-`, code and `-%>`.
((output_directive
  (code) @_comment_body) @comment
  (#match? @_comment_body "^-"))
