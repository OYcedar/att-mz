# Requisitos de razonamiento

Antes del JSON final, devuelve exactamente un bloque `<why>...</why>`.

- Empieza directamente con `<why>` exacto, en minúsculas y sin atributos, y termina con `</why>`.
- El contenido tras Unicode trim debe ser no vacío y analizar para cada `[ID]` el contexto, los
  referentes, la persona, el tono, la terminología, los saltos de línea, los ATT tokens, los restos
  de lengua fuente y el formato final.
- No escribas solo «comprobado» ni coloques el JSON final dentro de `<why>`.
- Después de `</why>` solo puede haber espacios antes del JSON exigido por el system Prompt.
