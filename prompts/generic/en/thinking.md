# Thinking output requirements

Before the final JSON, output exactly one `<why>...</why>` block.

- Start the response with the exact lowercase, attribute-free `<why>` and end it with `</why>`.
- Its Unicode-trimmed content must be non-empty and analyze context, references, person, tone,
  terminology, line breaks, ATT tokens, source-language residue, and final formatting for each
  `[ID]`.
- Do not merely say that checks were performed, and do not put the final JSON inside `<why>`.
- After `</why>`, allow whitespace only, then output the JSON required by the system Prompt.
