app-about = Traduce juegos de RPG Maker con un estado de proyecto reutilizable
cli-config-help = Archivo de configuración TOML estricto para esta ejecución
cli-ui-language-help = Idioma de la ayuda, diagnósticos, progreso, resultados y registros del proyecto: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko o vi
cli-progress-help = Modo de progreso en vivo: auto, plain u off
cli-mz-about = Traduce un juego de RPG Maker MZ
cli-mv-about = Traduce un juego de RPG Maker MV
cli-init-about = Inicializa o actualiza un proyecto de juego con nombre
cli-extract-about = Extrae texto con un plan owner explícito o guardado
cli-translate-about = Traduce el texto extraído con un Profile explícito o guardado
cli-write-back-about = Escribe las traducciones aceptadas en el juego
cli-project-lua-about = Ejecuta una vez un programa Lua de confianza en el contexto del proyecto
cli-project-name-help = Nombre estable del proyecto
cli-init-path-help = Raíz del juego RPG Maker; un proyecto existente puede reutilizar su última ruta correcta
cli-source-language-help = ID del idioma de origen
cli-target-language-help = ID del idioma de destino
cli-dialogue-width-help = Máximo de caracteres de ancho completo por línea de diálogo
cli-scrolling-width-help = Máximo de caracteres de ancho completo por línea de texto desplazable
cli-help-width-help = Máximo de caracteres de ancho completo por línea de ayuda o descripción
cli-builtin-help = Usa las ubicaciones de texto RPG Maker integradas en ATT
cli-rules-help = Sustituye el owner Rules por esta definición TOML; una lista vacía lo desactiva
cli-dialogue-rules-help = Sustituye la proyección de nombres de diálogo MV usada con Builtin
cli-lua-help = Sustituye el programa Lua de la fase; un archivo de cero bytes lo elimina
cli-profile-help = ID del Profile de traducción; omítelo para reutilizar el último Profile correcto
cli-terms-help = Sustituye el recurso terminológico del proyecto
cli-placeholders-help = Sustituye el recurso Placeholder del proyecto
cli-project-lua-profile-help = Profile para la validación manual Standard; si se omite, el último Profile Translate correcto se resuelve al abrir Standard
cli-project-lua-script-help = Programa Lua de confianza que se ejecutará una vez
cli-project-lua-arguments-help = Argumento UTF-8 pasado a Lua arg[1..] después de --
cli-usage-heading = Uso:
cli-commands-heading = Comandos:
cli-options-heading = Opciones:
cli-arguments-heading = Argumentos:
cli-options-metavar = OPCIONES
cli-command-metavar = COMANDO
cli-print-help = Muestra la ayuda
cli-print-version = Muestra la versión
cli-missing-config = Falta la ruta de configuración obligatoria --config <FILE>.
cli-blank-value = El valor no puede estar vacío.
cli-invalid-positive-integer = El valor debe ser un entero positivo.
cli-invalid-progress = El modo de progreso { $value } no está admitido; usa auto, plain u off.
cli-invalid-ui-language-argument = --ui-language contiene una etiqueta de idioma no válida: { $value }.
cli-unsupported-ui-language-argument = --ui-language solicita un idioma no admitido: { $value }.
cli-invalid-ui-language-environment = ATT_UI_LANGUAGE contiene una etiqueta de idioma no válida: { $value }.
cli-unsupported-ui-language-environment = ATT_UI_LANGUAGE solicita un idioma no admitido: { $value }.
cli-ui-language-environment-not-unicode = ATT_UI_LANGUAGE no es Unicode válido.
cli-unexpected-argument = Argumento inesperado: { $value }.
cli-missing-required-argument = Falta un argumento obligatorio: { $value }.
cli-invalid-value = El valor { $value } no es válido para { $argument }.
cli-error-heading = Error:
cli-try-help = Para obtener más información, usa --help.
cli-missing-value = Se requiere un valor para { $argument }.
cli-missing-subcommand = Se requiere un comando.
cli-argument-conflict = { $argument } no puede usarse con los demás argumentos proporcionados.
cli-wrong-number-of-values = Se proporcionó un número incorrecto de valores para { $argument }.
cli-invalid-utf8 = Un argumento de la línea de comandos no es Unicode válido.
cli-parse-failure = No se pudo analizar la línea de comandos.
log-label-phase-check-project = comprobación del proyecto
log-label-phase-scan-source = análisis del origen
log-label-phase-prepare-candidate = preparación del candidato
log-label-phase-update-database = actualización de la base de datos
log-label-phase-publish = publicación
log-label-phase-builtin = extracción integrada
log-label-phase-rules = extracción por reglas
log-label-phase-lua = procesamiento Lua
log-label-phase-planning = planificación
log-label-phase-confirmed-tasks = confirmación de tareas
log-label-phase-no-work = sin trabajo necesario
log-label-phase-read-assets = lectura de recursos
log-label-phase-plan-standard = planificación de escritura estándar
log-label-phase-rewrite-documents = reescritura de documentos
log-label-phase-validate-candidate = validación del candidato
log-label-task-complete = completo
log-label-task-partial = parcial
log-label-task-unavailable = no disponible
log-label-task-failed = fallido
error-state-applied-finalization = El resultado surtió efecto, pero falló la finalización. Revisa el estado del proyecto antes de reintentar.
error-no-executable-extract-owner = Tras limpiar no queda ningún owner Extract ejecutable, por lo que el plan no se guardó.
error-plan-save-failed-applied = El resultado surtió efecto, pero el nuevo plan no se guardó. La próxima vez indica explícitamente las opciones deseadas.
error-plan-save-outcome-unknown = El resultado surtió efecto, pero no se puede confirmar el commit del plan. La próxima vez indica explícitamente las opciones deseadas.
plan-source-explicit = entrada explícita
plan-source-project-state = estado del proyecto
plan-source-product-default = comportamiento del producto
notice-init-reuse-path = No se indicó una ruta de origen; se reutiliza la última ruta correcta: { $path }.
notice-extract-reuse-owners = No se indicó el ámbito de extracción; se reutiliza el último plan correcto: { $owners }.
notice-translate-reuse-profile = No se indicó Profile; se reutiliza el último Profile correcto: { $profile }.
notice-translate-reuse-lua = No se indicó una opción Lua; se reutiliza la última selección correcta de Translate Lua.
notice-write-back-reuse-lua = No se indicó opción Lua; se reutiliza el último programa WriteBack Lua correcto.
notice-write-back-standard-only = No hay programa WriteBack Lua configurado; solo se ejecutará Standard.
notice-owner-disabled = El owner { $owner } se desactivó y se quitó de futuros planes automáticos.
notice-lua-cleared = Se eliminó el programa Lua { $phase }; no se ejecutará esta vez.
notice-no-model-request = Todas las unidades de traducción estándar están actualizadas; Standard no envió ninguna solicitud al modelo en esta ejecución.
notice-manual-layout = { $count ->
    [one] 1 unidad necesita revisión manual de saltos de línea.
   *[other] { $count } unidades necesitan revisión manual de saltos de línea.
}
notice-log-degraded = El registro del proyecto no está disponible o está degradado; el comando continúa y su estado de salida no cambia.
progress-init-check-project = Comprobando el estado del proyecto
progress-init-scan-source = Explorando el origen del juego
progress-init-build-candidate = Construyendo el candidato del proyecto
progress-init-converge-database = Convergiendo la base de datos del proyecto
progress-init-publish = Publicando el proyecto inicializado
progress-save-run-plan = Guardando el plan de ejecución correcto
progress-extract-owner = Owner de extracción: { $owner }
progress-extract-documents = Explorando documentos
progress-extract-builtin = Unidades Builtin
progress-extract-rules = Definiciones Rules
progress-extract-lua = Ejecutando el programa Extract Lua
progress-extract-commit = Confirmando los recursos extraídos
progress-translate-planning = Planificando tareas de traducción
progress-translate-confirmed = Tareas de traducción confirmadas
progress-translate-no-work = No hace falta solicitar el modelo
progress-project-lua = Ejecutando el programa Lua del proyecto
progress-write-back-read-assets = Leyendo recursos aceptados
progress-write-back-planning = Planificando la reescritura de documentos
progress-write-back-documents = Documentos reescritos
progress-write-back-lua = Ejecutando el programa WriteBack Lua
progress-write-back-validate-candidate = Validando el candidato de salida
progress-write-back-publish = Publicando la salida; una interrupción esperará un resultado confirmado
progress-finalizing = Finalizando los recursos obligatorios
progress-safe-stopping = Deteniendo de forma segura; se conserva el último progreso confirmado
result-init-completed = Inicialización completa: { $project }
result-init-created = Estado del proyecto: creado
result-init-unchanged = Estado del proyecto: sin cambios
result-init-updated = Estado del proyecto: actualizado
result-init-stale-owners = Se requiere volver a extraer: { $owners }
result-extract-completed = Extracción completa: { $project }
result-translate-completed = Traducción completa: { $project } (Profile: { $profile })
result-translate-standard = Traducción estándar: { $total } tareas; { $complete } completas, { $partial } parciales, { $unavailable } no disponibles; { $written } ubicaciones escritas, { $remaining } restantes
result-translate-convergence = Convergencia: { $retained } conservadas, { $invalidated } invalidadas, { $not_applicable } no aplicables, { $reused } reutilizadas
result-write-back-completed = Escritura completa: { $project }
result-project-lua-completed = Ejecución Lua del proyecto completada: { $project }
result-output-directory = Directorio de salida: { $path }
result-write-back-standard = Escritura estándar: { $translated } unidades traducidas, { $original } unidades de origen; { $auto_wrapped } ajustes automáticos, { $breaks } saltos y { $indents } sangrías de ancho completo añadidos; { $manual } diseños manuales
result-lua-executed = Lua: ejecutado
result-lua-not-executed = Lua: no ejecutado
result-cancelled = El comando se canceló tras finalizar de forma segura.
result-plan-saved = Se guardó el plan de ejecución correcto.
result-translate-plan-sources = Se guardó el plan de esta ejecución correcta. Origen del Profile: { $profile_source }; origen de Lua: { $lua_source }.
log-run-started = El comando { $command } comenzó.
log-run-succeeded = El comando { $command } terminó correctamente.
log-run-failed = El comando { $command } falló.
log-run-outcome-unknown = El comando { $command } terminó con un resultado final desconocido; siga las ubicaciones de recuperación indicadas en el error.
log-run-cancelled = El comando { $command } se canceló.
log-performance-counters = Contadores de rendimiento: { $sqlite_control_attempted_total } intentos de control de transacciones SQLite; validaciones completas del árbol candidato iniciadas { $candidate_validation_started }, completadas { $candidate_validation_completed }.
log-plan-resolved = El plan de { $command } procede de { $source }.
log-phase-started = Fase iniciada: { $phase }.
log-phase-finished = Fase terminada: { $phase }.
log-retry-summary = { $count ->
    [one] Se realizó 1 reintento.
   *[other] Se realizaron { $count } reintentos.
}
log-no-work = No se necesitó trabajo: { $reason }.
log-no-work-translation-up-to-date = las traducciones ya coinciden con el origen y el perfil actuales
log-partial-result = { $count ->
    [one] 1 resultado parcial requiere atención.
   *[other] { $count } resultados parciales requieren atención.
}
log-translation-task-started = Tarea de traducción { $index }/{ $total } iniciada.
log-translation-task-finished = Tarea de traducción { $index } terminada con resultado { $outcome }.
log-translation-task-diagnostic = La tarea de traducción { $index } informó un diagnóstico tras { $attempts } intentos: { $diagnostic }
diagnostic-title = Error [{ $code }]
diagnostic-stage = Etapa: { $stage }
diagnostic-subject = Ubicación: { $subject }
diagnostic-subject-value = { $kind ->
    [command] comando { $value }
    [field] campo { $value }
    [project] proyecto { $value }
    [profile] perfil { $value }
    [component] componente { $value }
   *[other] { $value }
}
diagnostic-reason = Motivo: { $reason }
diagnostic-impact = Impacto: { $impact }
diagnostic-action = Acción: { $action }
diagnostic-recovery = Recuperación: { $recovery }
diagnostic-recovery-value = { $kind ->
    [component] componente { $value }
    [transaction] transacción { $value }
   *[other] { $value }
}
diagnostic-related = Error relacionado { $index }:
diagnostic-stage-value = { $code ->
    [process_output] Salida del proceso
    [lua] Ejecución Lua del proyecto
   *[other] { $fallback }
}
diagnostic-impact-value = { $code ->
   *[other] { $fallback }
}
diagnostic-action-value = { $code ->
   *[other] { $fallback }
}
diagnostic-failure-value = { $code ->
   *[other] { $fallback }
}
diagnostic-io-kind-value = { $code ->
   *[other] { $fallback }
}
diagnostic-configuration-rule-value = { $code ->
   *[other] { $fallback }{ $facts }
}
task-record-title = Tarea de traducción { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] Completada
    [partial] Parcialmente completada
    [unavailable] No disponible
    [execution_failed] Error de ejecución
    [commit_preparation_failed] Error al preparar el commit
    [commit_not_applied] Commit no aplicado
    [commit_outcome_unknown] Resultado del commit desconocido
    [not_committed_after_earlier_failure] Sin commit tras un error anterior
    [invalid_result] Secuencia de resultados de Executor no válida
    [cancelled] Cancelada
   *[other] { $state }
}
task-record-summary-with-written = `Tarea { $ordinal }/{ $total }` · `{ $attempts } intentos` · `Aceptadas { $accepted }/{ $expected }` · `Escritas en { $written } ubicaciones`
task-record-summary-without-written = `Tarea { $ordinal }/{ $total }` · `{ $attempts } intentos` · `Aceptadas { $accepted }/{ $expected }`
task-record-run-id-label = ID de ejecución:
task-record-started-at-label = Inicio:
task-record-duration-label = Duración total:
task-record-endpoint-label = Endpoint:
task-record-model-label = Modelo:
task-record-custom-parameters-heading = Parámetros personalizados
task-record-attempts-heading = Intentos de solicitud
task-record-final-result-heading = Resultado final
task-record-no-request = No se generó una solicitud de modelo lista para enviar.
task-record-empty-assistant = El modelo devolvió un objeto vacío.
task-record-parse-error = Error de análisis: { $kind ->
    [json] JSON de respuesta del modelo no válido (categoría `{ $category }`), línea { $line }, columna { $column }
    [thinking_not_allowed] este modo de respuesta no acepta razonamiento, línea { $line }, columna { $column }
    [thinking_envelope_missing] falta el sobre de razonamiento obligatorio, línea { $line }, columna { $column }
    [thinking_envelope_unclosed] el sobre de razonamiento no está cerrado, línea { $line }, columna { $column }
    [thinking_empty] el contenido del razonamiento está vacío, línea { $line }, columna { $column }
    [thinking_nested] hay un sobre de razonamiento anidado, línea { $line }, columna { $column }
    [thinking_repeated] hay un sobre de razonamiento repetido, línea { $line }, columna { $column }
    [markdown_fence_no_body] el bloque Markdown no tiene contenido, línea { $line }, columna { $column }
    [markdown_fence_unsupported] solo se acepta un bloque Markdown sin etiqueta de idioma o con etiqueta json, línea { $line }, columna { $column }
    [markdown_fence_unclosed] el bloque Markdown no está cerrado, línea { $line }, columna { $column }
   *[markdown_fence_invalid_closing] el bloque Markdown debe cerrarse en la última línea independiente, línea { $line }, columna { $column }
}
task-record-attempt-succeeded = Intento { $number }: correcto; finish reason { $finish_reason }
task-record-attempt-token-usage = ; tokens `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ; duración `{ $duration }`
task-record-attempt-request-id = ; request ID { $request_id }
task-record-attempt-response-id = ; response ID { $response_id }
task-record-attempt-retryable = Intento { $number }: error reintentable; diagnóstico `{ $code }`; duración `{ $duration }`
task-record-attempt-retry-after = ; Retry-After `{ $duration }`
task-record-attempt-wait-retry = ; reintento tras `{ $duration }`
task-record-attempt-wait-completed = ; espera de `{ $duration }` completada; el siguiente intento no comenzó
task-record-attempt-wait-cancelled = ; espera prevista de `{ $duration }`; cancelado durante la espera
task-record-attempt-failed = Intento { $number }: error al procesar la solicitud o respuesta; diagnóstico `{ $code }`; duración `{ $duration }`
task-record-attempt-cancelled = Intento { $number }: cancelado; duración `{ $duration }`
task-record-structured-reason = Motivo: { $reason }
task-record-final-status = Estado: { $state ->
    [complete] completada y commit confirmado
    [partial] parcialmente completada y commit confirmado
    [unavailable] no disponible; proyecto sin cambios
    [execution_failed] error de ejecución; sin commit
    [commit_preparation_failed] error al preparar el commit; no aplicado con certeza
    [commit_not_applied] transacción no aplicada con certeza
    [commit_outcome_unknown] resultado del commit desconocido
    [not_committed_after_earlier_failure] sin commit porque falló una tarea anterior
    [invalid_result] secuencia de resultados de Executor no válida; sin commit
    [cancelled] cancelada; sin commit
   *[other] { $state }
}
task-record-accepted-written = Aceptadas: { $accepted } entradas, escritas en { $written } ubicaciones reales
task-record-accepted-outcome-unknown = Validadas: { $accepted } entradas; no se puede confirmar el resultado del commit de la base de datos
task-record-rejected-heading = No aceptadas:
task-record-rejected-item = { $id }: { $reason }
task-record-protocol-diagnostic = Diagnóstico de protocolo: { $diagnostic }
task-record-unavailable-reason = Motivo de indisponibilidad: { $reason }
task-record-task-diagnostic = Diagnóstico de tarea: `{ $code }`; motivo { $reason }
task-record-rejection-reason = { $code ->
    [missing] Falta la salida del modelo
    [duplicate] Salida del modelo duplicada
    [invalid_shape] { $detail }
    [invalid_shape_array] La traducción debe ser una matriz de cadenas
    [invalid_shape_item] El elemento { $line } de la matriz de traducción debe ser una cadena
    [line_count_mismatch] El número de líneas no coincide (esperado { $expected }, real { $actual })
    [invalid_line_text] La línea { $line } contiene caracteres de control no válidos
    [blank_line_mismatch] El estado en blanco no coincide en la línea { $line } (esperado: { $expected_blank ->
        [blank] en blanco
       *[other] no en blanco
    })
    [blank_translation] La traducción está en blanco
    [no_natural_language_text] La traducción no contiene texto en lenguaje natural
    [contains_byte_order_mark] La traducción contiene un BOM
    [placeholder_mismatch] Marcador de posición no coincidente: { $detail }
    [unexpected_placeholder] Marcador de posición inesperado: { $detail }
    [placeholder_normalization_ambiguous] Normalización ambigua del marcador: { $detail }
    [source_residual] Se detectó texto residual del idioma de origen: { $detail }
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason no es stop: { $detail }
    [invalid_response] { $detail }
    [invalid_id] El elemento { $index } del modelo tiene un ID no válido
    [unknown_id] El elemento { $index } del modelo devolvió el ID desconocido { $detail }
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] No se pudo analizar la respuesta del modelo
    [all_outputs_rejected] Todas las salidas del modelo fueron rechazadas
    [recoverable_request_exhausted] Se agotó el presupuesto de reintentos recuperables
    [retry_after_exceeds_maximum] Retry-After supera la espera máxima configurada
   *[other] { $code }
}
task-record-duration-seconds = { $value } segundos
task-record-duration-milliseconds = { $value } ms
