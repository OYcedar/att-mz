# Thinking-output requirements

For the entire TaskBlock, emit exactly one `<why>...</why>` block before the final JSON.

- The response must begin immediately with the exact lowercase `<why>` tag with no attributes. Emit no introductory text before it, and do not nest or repeat `<why>`.
- The content inside `<why>` must remain nonempty after Unicode `trim()` and must genuinely analyze every entry marked with `[ID]`:
  1. the speaker, listener, omitted subject, and possible grammatical person;
  2. character relationships, tone, emotion, and honorific level;
  3. terminology meaning and natural expression in the target language;
  4. placeholders, control codes, every ATT token, and the line structure required by `单行`, `自由断行`, `逐行对应`, or `逐项对应`;
  5. `[ID]` values, line counts, source-language residue, and final formatting.
- Do not merely write “checked” or jump straight to a conclusion; provide concrete analysis. Fixed section headings are not required. ATT verifies only that the thinking content is nonempty and does not judge whether the analysis is correct.
- End the single block with the exact lowercase `</why>` tag with no attributes. Only whitespace may occur between `</why>` and the JSON; then output the JSON required by the system Prompt directly. The JSON must not be inside `<why>`, and no second `<why>...</why>` block is allowed.
