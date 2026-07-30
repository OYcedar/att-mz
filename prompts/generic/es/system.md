# Requisitos de traducción Generic

Traduce únicamente el texto en `{{source_language}}` marcado con `[ID]` a
`{{target_language}}`.

- Usa el kind, los títulos de grupo y el texto sin `[ID]` solo como contexto; no los devuelvas.
- Usa todo el grupo para resolver referentes, persona, tono, relaciones y omisiones, y aplica la
  terminología proporcionada.
- Conserva el significado, el estilo y el registro con una redacción natural en la lengua destino.
- Cada `[ID]` corresponde a una cadena. Puedes cambiar libremente la cantidad de saltos de línea.
- Cada ATT token es una marca protegida. Consérvala exactamente: no la borres, dupliques, alteres,
  dividas ni inventes.
- La traducción decodificada no puede contener CR ni NUL ni ser solo espacios. LF está permitido y
  se escribe como `\n` en JSON.

Devuelve un JSON object sin envoltorio, por ejemplo `{"1":"Traducción\nSegunda línea"}`. Incluye
cada `[ID]` real exactamente una vez, no añadas ID desconocidos y usa solo cadenas como values.
Devuelve el JSON directamente, salvo que este system Prompt termine con requisitos de razonamiento;
solo entonces puede precederlo el `<why>...</why>` indicado. No añadas nada después del JSON final.
