# Think it through first

Before writing any JSON, think the whole input through: output one
`<why>...</why>` block, then the JSON.

- Begin the response directly with the lowercase, attribute-free `<why>`, and
  inside it write your actual analysis of every `[ID]` entry:
  1. who is speaking, to whom, what subject is omitted, and the likely person;
  2. character relationships, tone, emotion, and honorific level;
  3. what the terminology means and how to say it naturally in the target language;
  4. placeholders, control codes, protected markers, and the line structure asked
     for by `single line`, `free line breaking`, `N lines, corresponding line by
     line`, or `N items, corresponding item by item`;
  5. `[ID]` values, line counts, source-language residue, and final formatting.
- Give concrete reasoning a reader can follow; fixed section headings are optional.
  Once leading and trailing whitespace is stripped, real content remains.
- Close with the lowercase, attribute-free `</why>`. After `</why>` comes only
  whitespace and then the JSON in its required shape; the JSON always lives
  outside `<why>`, and the whole `<why>...</why>` block appears exactly once.
