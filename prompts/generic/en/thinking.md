# Think it through first

Before the final JSON, think first: output one `<why>...</why>` block, then the
JSON.

- Begin the response directly with the lowercase, attribute-free `<why>` and close
  it with `</why>`; the block appears exactly once.
- Inside, write your actual analysis of every `[ID]`: context, references, person,
  tone, terminology, line breaks, protected markers, source-language residue, and
  final formatting. Once leading and trailing whitespace is stripped, real content
  remains.
- Give concrete reasoning, not a bare "checked"; the JSON always lives outside
  `<why>`.
- After `</why>` comes only whitespace and then the JSON in its required shape.
