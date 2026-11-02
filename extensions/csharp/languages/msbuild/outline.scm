(element
  (STag
    (Name) @context
    (Attribute
      (Name) @_attribute
      (AttValue) @name)
    (#eq? @context "Target")
    (#eq? @_attribute "Name"))) @item

(element
  (STag
    (Name) @context
    (Attribute
      (Name) @_attribute
      (AttValue) @name)
    (#match? @context "^(PackageReference|ProjectReference|Project)$")
    (#match? @_attribute "^(Include|Path)$"))) @item
