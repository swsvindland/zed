((content) @injection.content
  (#set! injection.language "ASPX Markup")
  (#set! injection.combined))

; Statement blocks are combined so that `<% if (x) { %> … <% } %>` parses as one C# program.
((directive
  (code) @injection.content)
  (#not-match? @injection.content "^[@$:]")
  (#set! injection.language "cs")
  (#set! injection.combined))

; <%: Html.Encoded(value) %>
((directive
  (code) @injection.content)
  (#match? @injection.content "^:")
  (#set! injection.language "cs"))

; <%= value %>
((output_directive
  (code) @injection.content)
  (#not-match? @injection.content "^-")
  (#set! injection.language "cs"))

; Data binding: <%# Eval("Name") %> and <%#: Item.Name %>
((comment_directive
  (comment) @injection.content)
  (#set! injection.language "cs"))
