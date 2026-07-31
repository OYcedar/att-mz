# Piénsalo bien primero

Antes de escribir cualquier JSON, analiza toda la entrada: genera un bloque
`<why>...</why>` y después el JSON.

- Empieza la respuesta directamente con `<why>`, en minúsculas y sin atributos, y
  dentro escribe tu análisis real de cada entrada con `[ID]`:
  1. quién habla, a quién, qué sujeto se omite y cuál es la persona probable;
  2. las relaciones entre personajes, el tono, la emoción y el nivel de
     tratamiento honorífico;
  3. qué significa la terminología y cómo decirla con naturalidad en el idioma
     de destino;
  4. los marcadores de posición, los códigos de control, los marcadores
     protegidos y la estructura de líneas que piden `single line`,
     `free line breaking`, `N lines, corresponding line by line` o
     `N items, corresponding item by item`;
  5. los valores `[ID]`, el número de líneas, los restos del idioma de origen y
     el formato final.
- Ofrece un razonamiento concreto que el lector pueda seguir; los encabezados de
  sección fijos son opcionales. Una vez eliminados los espacios del principio y
  del final, debe quedar contenido de verdad.
- Cierra con `</why>`, en minúsculas y sin atributos. Después de `</why>` solo
  puede haber espacios en blanco y luego el JSON con la forma exigida; el JSON
  vive siempre fuera de `<why>`, y el bloque `<why>...</why>` completo aparece
  exactamente una vez.
