# Requisitos de salida del razonamiento

Para todo el TaskBlock, genera exactamente un bloque `<why>...</why>` antes del JSON final.

- La respuesta debe comenzar inmediatamente con la etiqueta exacta `<why>`, en minúsculas y sin atributos. No generes texto introductorio antes de ella ni anides o repitas `<why>`.
- El contenido de `<why>` debe seguir sin estar vacío después de Unicode `trim()` y debe analizar realmente cada entrada marcada con `[ID]`:
  1. el hablante, el oyente, el sujeto omitido y la posible persona gramatical;
  2. las relaciones entre personajes, el tono, la emoción y el nivel de tratamiento honorífico;
  3. el significado de la terminología y su expresión natural en el idioma de destino;
  4. los marcadores de posición, los códigos de control, cada ATT token y la estructura de líneas exigida por `单行`, `自由断行`, `逐行对应` o `逐项对应`;
  5. los valores `[ID]`, el número de líneas, los restos del idioma de origen y el formato final.
- No te limites a escribir «comprobado» ni pases directamente a una conclusión; proporciona un análisis concreto. No se exigen títulos de sección fijos. ATT solo comprueba que el contenido del razonamiento no esté vacío y no juzga si el análisis es correcto.
- Termina el único bloque con la etiqueta exacta `</why>`, en minúsculas y sin atributos. Entre `</why>` y el JSON solo puede haber espacios en blanco; después, genera directamente el JSON exigido por el system Prompt. El JSON no debe estar dentro de `<why>` y no se permite un segundo bloque `<why>...</why>`.
