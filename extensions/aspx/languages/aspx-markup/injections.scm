((comment) @injection.content
  (#set! injection.language "comment"))

(script_element
  (start_tag) @_start_tag
  (raw_text) @injection.content
  (#not-match? @_start_tag "(?i)runat\\s*=\\s*[\"']?server")
  (#set! injection.language "javascript"))

; <script runat="server"> holds the page's C# code.
(script_element
  (start_tag) @_start_tag
  (raw_text) @injection.content
  (#match? @_start_tag "(?i)runat\\s*=\\s*[\"']?server")
  (#set! injection.language "cs"))

(style_element
  (raw_text) @injection.content
  (#set! injection.language "css"))

(attribute
  (attribute_name) @_attribute_name
  (#match? @_attribute_name "^style$")
  (quoted_attribute_value
    (attribute_value) @injection.content)
  (#set! injection.language "css"))

; `onclick` is client script, while server controls' `OnClick` names a C# event handler.
(attribute
  (attribute_name) @_attribute_name
  (#match? @_attribute_name "^(on[a-z]+|OnClientClick)$")
  (quoted_attribute_value
    (attribute_value) @injection.content)
  (#set! injection.language "javascript"))
