; Test methods (xUnit, NUnit, MSTest)
(class_declaration
  name: (identifier) @csharp_class_name
  body: (declaration_list
    (method_declaration
      (attribute_list
        (attribute
          name: (_) @_attribute_name
          (#match? @_attribute_name "(^|\.)(Fact|Theory|Test|TestCase|TestCaseSource|TestMethod|DataTestMethod)(Attribute)?$")))
      name: (identifier) @run @csharp_method_name)
    (#set! tag csharp-test)))

; Test classes
((class_declaration
  name: (identifier) @run @csharp_class_name
  body: (declaration_list
    (method_declaration
      (attribute_list
        (attribute
          name: (_) @_attribute_name
          (#match? @_attribute_name "(^|\.)(Fact|Theory|Test|TestCase|TestCaseSource|TestMethod|DataTestMethod)(Attribute)?$"))))))
  (#set! tag csharp-test-class))

; Entry points
((method_declaration
  name: (identifier) @run @_method_name)
  (#eq? @_method_name "Main")
  (#set! tag csharp-main))

; Top-level statements: mark the first statement only
((compilation_unit
  .
  (global_statement) @run)
  (#set! tag csharp-main))

((compilation_unit
  (using_directive)
  .
  (global_statement) @run)
  (#set! tag csharp-main))
