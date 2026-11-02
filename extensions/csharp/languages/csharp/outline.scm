(namespace_declaration
  "namespace" @context
  name: (_) @name) @item

(file_scoped_namespace_declaration
  "namespace" @context
  name: (_) @name) @item

(class_declaration
  "class" @context
  name: (identifier) @name) @item

(struct_declaration
  "struct" @context
  name: (identifier) @name) @item

(record_declaration
  "record" @context
  name: (identifier) @name) @item

(interface_declaration
  "interface" @context
  name: (identifier) @name) @item

(enum_declaration
  "enum" @context
  name: (identifier) @name) @item

(enum_member_declaration
  name: (identifier) @name) @item

(delegate_declaration
  "delegate" @context
  name: (identifier) @name
  parameters: (parameter_list) @context) @item

(constructor_declaration
  name: (identifier) @name
  parameters: (parameter_list) @context) @item

(method_declaration
  name: (identifier) @name
  parameters: (parameter_list) @context) @item

(property_declaration
  name: (identifier) @name) @item

(event_declaration
  "event" @context
  name: (identifier) @name) @item

(field_declaration
  (variable_declaration
    (variable_declarator
      name: (identifier) @name))) @item

(comment) @annotation
