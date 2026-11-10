; Sections directly under <configuration>, such as <appSettings>, <connectionStrings> and <system.web>.
(element
  (STag
    (Name) @_root)
  (content
    (element
      [
        (STag
          (Name) @name)
        (EmptyElemTag
          (Name) @name)
      ]) @item)
  (#eq? @_root "configuration")
  (#not-eq? @name "location"))

(element
  (STag
    (Name) @_root)
  (content
    (element
      (STag
        (Name) @context
        (Attribute
          (Name) @_attribute
          (AttValue) @name))) @item)
  (#eq? @_root "configuration")
  (#eq? @context "location")
  (#eq? @_attribute "path"))

; Keyed entries: <add key="…">, <add name="…">, <section name="…">, <assemblyIdentity name="…">, <rule name="…">.
(element
  [
    (STag
      (Name) @context
      (Attribute
        (Name) @_attribute
        (AttValue) @name))
    (EmptyElemTag
      (Name) @context
      (Attribute
        (Name) @_attribute
        (AttValue) @name))
  ]
  (#any-of? @_attribute "key" "name")) @item
