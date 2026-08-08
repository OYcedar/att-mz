app-about = Traduce juegos y texto estructurado con un estado de proyecto reutilizable
cli-ui-language-help = Idioma de la ayuda, diagnósticos, progreso, resultados y registros del proyecto: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko o vi
cli-mz-about = Traduce un juego de RPG Maker MZ
cli-mv-about = Traduce un juego de RPG Maker MV
cli-generic-about = Traduce texto JSONL estructurado
cli-init-about = Inicializa o actualiza un proyecto de traducción con nombre
cli-extract-about = Sincroniza el texto de origen desde la entrada actual del proyecto
cli-translate-about = Traduce el texto extraído con un Profile explícito o guardado
cli-write-back-about = Escribe las traducciones actuales en la salida del proyecto
cli-manual-about = Gestionar traducciones manuales en un archivo TOML editable
cli-manual-export-about = Exportar entradas que requieren traducción manual
cli-manual-check-about = Comprobar un TOML de traducciones sin modificar el proyecto
cli-manual-apply-about = Aplicar traducciones manuales completadas y válidas
cli-project-lua-about = Ejecutar un script Lua en la base de datos del proyecto
cli-project-name-help = Nombre estable del proyecto
cli-init-path-help = Directorio raíz de entrada; un proyecto existente puede reutilizar su última ruta correcta
cli-source-language-help = ID del idioma de origen
cli-target-language-help = ID del idioma de destino
cli-dialogue-width-help = Máximo de caracteres de ancho completo por línea de diálogo
cli-scrolling-width-help = Máximo de caracteres de ancho completo por línea de texto desplazable
cli-help-width-help = Máximo de caracteres de ancho completo por línea de ayuda o descripción
cli-builtin-help = Usa las ubicaciones de texto RPG Maker integradas en ATT
cli-rules-help = Sustituye las reglas de extracción de RPG Maker por esta definición TOML; una lista vacía las desactiva
cli-dialogue-rules-help = Sustituye la proyección de nombres de diálogo MV usada con Builtin
cli-profile-help = ID del Profile de traducción; omítelo para reutilizar el último Profile correcto
cli-terms-help = Sustituye el recurso terminológico del proyecto
cli-placeholders-help = Sustituye el recurso Placeholder del proyecto
cli-project-lua-script-help = Script Lua que se ejecutará en la base de datos del proyecto
cli-project-lua-arguments-help = Argumento UTF-8 pasado a Lua arg[1..] después de --
cli-manual-file-help = Archivo TOML de traducciones manuales
cli-usage-heading = Uso:
cli-commands-heading = Comandos:
cli-options-heading = Opciones:
cli-arguments-heading = Argumentos:
cli-options-metavar = OPCIONES
cli-command-metavar = COMANDO
cli-print-help = Muestra la ayuda
cli-print-version = Muestra la versión
cli-blank-value = El valor no puede estar vacío.
cli-invalid-positive-integer = El valor debe ser un entero positivo.
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
error-no-executable-extract-owner = Tras limpiar no queda ningún owner Extract ejecutable, por lo que el plan no se guardó.
plan-source-explicit = entrada explícita
plan-source-project-state = estado del proyecto
plan-source-product-default = comportamiento del producto
notice-init-reuse-path = No se indicó una ruta de origen; se reutiliza la última ruta correcta: { $path }.
notice-extract-reuse-owners = No se indicó el ámbito de extracción; se reutiliza el último plan correcto: { $owners }.
notice-translate-reuse-profile = No se indicó Profile; se reutiliza el último Profile correcto: { $profile }.
notice-owner-disabled = El owner { $owner } se desactivó y se quitó de futuros planes automáticos.
warning-rules-command-non-string-skipped = Advertencia: la regla Rules { $rule_number } omitió { $skipped_count } parámetros command que no eran cadenas (origen { $source_file }, code={ $command_code }, parameter={ $parameter }, tipo={ $actual_type }).
warning-manual-layout-required = Advertencia: revise manualmente los saltos de línea en { $locations } (region={ $region }, max_fullwidth_chars={ $max_fullwidth_chars }).
notice-no-model-request = Todas las unidades de traducción están actualizadas; esta ejecución no necesitó enviar solicitudes al modelo.
notice-manual-layout = { $count ->
    [one] 1 unidad necesita revisión manual de saltos de línea.
   *[other] { $count } unidades necesitan revisión manual de saltos de línea.
}
notice-log-degraded = El registro del proyecto no está disponible o está degradado; el comando continúa y su estado de salida no cambia.
notice-task-records-degraded = Los registros de tareas de traducción no están disponibles o están degradados; el comando continúa y su estado de salida no cambia.
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
progress-extract-commit = Confirmando los recursos extraídos
progress-generic-init = Inicializando el proyecto Generic
progress-generic-extract = Explorando la entrada JSONL Generic
progress-translate-planning = Planificando tareas de traducción
progress-translate-confirmed = Tareas de traducción confirmadas
progress-translate-no-work = No hace falta solicitar el modelo
progress-project-lua = Ejecutando el programa Lua del proyecto
progress-write-back-read-assets = Leyendo recursos aceptados
progress-write-back-planning = Planificando la reescritura de documentos
progress-write-back-documents = Documentos reescritos
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
result-translate-summary = Traducción: { $total } tareas; { $complete } completas, { $partial } parciales, { $unavailable } no disponibles; { $written } ubicaciones escritas, { $remaining } restantes
result-translate-convergence = Convergencia: { $retained } conservadas, { $invalidated } invalidadas, { $not_applicable } no aplicables, { $reused } reutilizadas
result-write-back-completed = Escritura completa: { $project }
result-project-lua-completed = Ejecución Lua del proyecto completada: { $project }
result-output-directory = Directorio de salida: { $path }
result-write-back-summary = Escritura: { $translated } unidades traducidas, { $original } unidades de origen; { $auto_wrapped } ajustes automáticos, { $breaks } saltos y { $indents } sangrías de ancho completo añadidos; { $manual } diseños manuales
result-generic-extract-unchanged = Entrada Generic sin cambios: { $files } archivos, { $groups } grupos, { $units } unidades
result-generic-extract-updated = Entrada Generic actualizada: { $files } archivos, { $groups } grupos, { $units } unidades; { $preserved } traducciones conservadas y { $cleared } borradas
result-generic-translate-summary = Traducción Generic: { $total } tareas; { $complete } completas, { $partial } parciales, { $unavailable } no disponibles; { $cleared } borradas, { $reused } reutilizadas, { $accepted } aceptadas, { $written } escritas, { $conflicted } conflictos, { $problems } problemas de respuesta
result-generic-write-back-summary = Escritura Generic: { $translated } unidades traducidas, { $original } unidades de origen conservadas
result-symbol-repair-summary = Reparación de símbolos: { $attempted } unidades examinadas, { $repaired } reparadas, { $skipped } omitidas internamente y { $replacements } símbolos sustituidos
result-cancelled = El comando se canceló tras finalizar de forma segura.
result-plan-saved = Se guardó el plan de ejecución correcto.
log-run-started = El comando { $command } comenzó.
log-run-succeeded = El comando { $command } terminó correctamente.
log-run-failed = El comando { $command } falló.
log-run-outcome-unknown = El comando { $command } terminó con un resultado final desconocido; siga las ubicaciones de recuperación indicadas en el error.
log-run-cancelled = El comando { $command } se canceló.
log-performance-counters = Contadores de rendimiento: { $sqlite_control_attempted_total } intentos de control de transacciones SQLite; validaciones completas del árbol candidato iniciadas { $candidate_validation_started }, completadas { $candidate_validation_completed }.
log-lua-print = Lua: { $message }
log-plan-resolved = El plan de { $command } procede de { $source }.
log-phase-started = Fase iniciada: { $phase }.
log-retry-summary = { $count ->
    [one] Se realizó 1 reintento.
   *[other] Se realizaron { $count } reintentos.
}
log-translation-task-started = Tarea de traducción { $index }/{ $total } iniciada.
log-translation-task-finished = Tarea de traducción { $index } terminada con resultado { $outcome }.
log-run-recovery-required = El comando { $command } terminó en un estado que requiere recuperación; siga las ubicaciones indicadas en el diagnóstico.
log-phase-completed = Fase completada: { $phase }.
log-phase-stopped = { $outcome ->
    [failed] La fase falló: { $phase }.
    [cancelled] Fase cancelada: { $phase }.
   *[other] Fase detenida: { $phase }.
}
log-cancellation-requested = Se solicitó la cancelación tras confirmar { $confirmed } de { $total } elementos.
log-cancellation-requested-indeterminate = Se solicitó la cancelación tras confirmar { $confirmed } elementos; se desconoce el total.
log-run-plan-finalized = { $result ->
    [saved] El plan de ejecución se guardó.
    [not_saved] El plan de ejecución no se guardó.
    [saved_finalization_failed] El plan se guardó, pero falló la finalización.
    [outcome_unknown] Se desconoce el estado final del plan de ejecución.
   *[other] La finalización del plan se detuvo sin un resultado reconocido.
}
log-translation-finished = { $result ->
    [not_started] La traducción no comenzó.
    [no_work] La traducción terminó sin trabajo pendiente.
    [complete] La traducción se completó.
    [incomplete] La traducción terminó con trabajo incompleto.
    [failed] La traducción falló.
    [cancelled] La traducción se canceló.
   *[other] La traducción se detuvo sin un resultado reconocido.
}
log-publication-started = Se inició la publicación en la raíz de salida { $path }.
log-publication-finished = { $result ->
    [published] La publicación se completó.
    [not_published] La publicación no modificó la salida.
    [recovery_required] La publicación se detuvo y requiere recuperación.
    [outcome_unknown] Se desconoce el estado final de la publicación.
   *[other] La publicación se detuvo sin un resultado reconocido.
}
log-project-log-degraded = El registro del proyecto se degradó; se registraron { $failure_kinds } categorías de fallo.
log-task-outcome-value = { $outcome ->
    [complete] completada
    [partial] completada parcialmente
    [unavailable] no disponible
    [failed] fallida
    [not_committed_after_earlier_failure] sin commit tras un error anterior
    [cancelled] cancelada
   *[other] terminada sin un resultado reconocido
}
diagnostic-location = Ubicación: { $subject }
diagnostic-explanation = Motivo: { $reason }
diagnostic-resolution = Acción: { $action }
diagnostic-related = Error relacionado { $index }:
diagnostic-resolution-value = { $code ->
    [fix_configuration] Corrige el campo de configuración indicado y vuelve a intentarlo
    [fix_input] Corrige la entrada indicada y vuelve a intentarlo
    [fix_placeholder_rules] Corrige la regla Placeholder indicada y vuelve a intentarlo
    [adjust_manual_layout] Ajusta manualmente los saltos de línea y el diseño en las ubicaciones indicadas según el ancho de pantalla señalado
    [check_path_and_permissions] Comprueba la ruta, el estado del sistema de archivos y los permisos
    [check_project_state] Revisa y corrige el estado del proyecto y vuelve a intentarlo
    [resolve_contention] Espera a que termine la operación en conflicto y vuelve a intentarlo
    [check_model_service] Comprueba la respuesta del servicio de modelos y los límites de la cuenta
    [preserve_recovery_artifacts] No elimines los artefactos de recuperación indicados; recupera la salida antes de volver a intentarlo
    [retry] Vuelve a intentar la operación
    [report_bug] Informa de este defecto de ATT y describe la operación que realizabas
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] Falta un valor obligatorio
    [generic_extract_required] La entrada JSONL ya no coincide con el último Extract; vuelve a ejecutar att generic extract
    [conflicting_values] Los valores proporcionados son incompatibles
    [invalid_syntax] La sintaxis del valor no es válida
    [invalid_encoding] La codificación del texto no es válida
    [invalid_value] El valor incumple el contrato requerido
    [not_found] El objeto requerido no existe
    [state_mismatch] El estado guardado del proyecto no satisface esta operación
    [unsupported_windows_code_page] La página de códigos de Windows no es UTF-8
    [transaction_rolled_back] La transacción falló y sus cambios se revirtieron
    [transaction_outcome_unknown] La transacción terminó sin confirmar la aplicación ni la reversión
    [finalization_failed] El resultado de la operación existe, pero la finalización falló
    [rollback_failed] Fallaron tanto la operación principal como la reversión
    [external_service_rejected] El servicio externo rechazó la solicitud
    [external_service_unavailable] El servicio externo no está disponible
    [executor_closed] El servicio de ejecución se está cerrando o ya está cerrado
    [concurrent_shutdown] Otro solicitante ya está cerrando el ejecutor
    [executor_state_poisoned] El estado del ciclo de vida del ejecutor está dañado
    [worker_spawn_failed] El sistema operativo no pudo crear el hilo de trabajo
    [worker_channel_closed] El canal de comandos del worker se cerró antes de terminar la finalización
    [worker_panicked] Un worker terminó de forma inesperada
    [reparse_point_forbidden] La ruta contiene un punto de reanálisis que no es de confianza
    [non_local_volume] La ruta no está en un volumen fijo local
    [non_ntfs_volume] La ruta no está en un volumen NTFS
    [case_sensitive_directory] El directorio usa una semántica de nombres que distingue mayúsculas
    [lock_cancelled] Se canceló la espera del bloqueo requerido
    [target_already_exists] El destino ya existe
    [file_identity_changed] La identidad del archivo cambió durante la operación
    [invalid_path] La ruta no es un destino válido para esta operación
    [wrong_publisher_instance] El token de publicación pertenece a otra instancia del publicador
    [journal_corrupt] El diario de recuperación de publicación no es válido o está incompleto
    [unexpected_artifact] Un artefacto inesperado del sistema de archivos bloquea la operación
    [interactive_session_already_open] Ya hay otra sesión interactiva de SQLite activa
    [backup_incomplete] La copia de seguridad de SQLite no llegó a completarse
    [request_serialization_failed] No se pudo serializar la solicitud al modelo
    [response_parsing_failed] La respuesta del modelo no es JSON válido
    [invalid_response_contract] La respuesta del modelo no cumple el contrato requerido
    [transport_failed] El transporte HTTP falló antes de recibir una respuesta válida
    [lua_compilation_failed] No se pudo compilar el programa Lua principal
    [lua_execution_failed] El programa Lua principal falló durante la ejecución
    [rules_pattern_match_failed] No se pudo evaluar el patrón PCRE2 de Rules
    [rules_zero_width_match] El patrón Rules produjo una coincidencia de ancho cero
    [rules_overlapping_capture] El patrón Rules produjo capturas de texto superpuestas
    [rules_missing_text_capture] La captura de texto con nombre requerida no participó en la coincidencia
    [rules_invalid_capture_range] La coincidencia o captura de Rules está fuera de los límites válidos de caracteres UTF-8
    [write_back_candidate_invalid] El candidato de reescritura no cumple la estructura de árbol data/js requerida
    [write_back_recovery_required] Hay que recuperar el directorio de salida antes de confiar en su contenido
    [already_exists] El objeto de destino ya existe
    [cancelled] La operación se canceló
    [concurrent_modification] El estado del proyecto cambió de forma simultánea
    [duplicate_identifier] Hay un identificador duplicado
    [extraction_out_of_date] La extracción guardada ya no coincide con la fuente actual
    [invalid_content] El contenido incumple el contrato requerido
    [manual_layout_required] Se requiere ajustar manualmente los saltos de línea o el diseño
    [operation_failed] La operación falló
    [placeholder_projection_failed] La proyección de Placeholder no conservó la estructura requerida
    [profile_not_found] El Profile de traducción seleccionado no existe
    [recovery_required] Es necesario recuperar el estado antes de confiar en el resultado
    [resource_limit] Se alcanzó un límite de recursos necesario
    [resource_limit_exceeded] La operación superó un límite de recursos del servicio
    [source_snapshot_mismatch] La fuente ya no coincide con la instantánea guardada
    [unavailable] El trabajo solicitado no está disponible temporalmente
    [internal_invariant] Se incumplió un invariante interno; se trata de un defecto de ATT
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] El término de política lingüística no puede estar vacío
    [language_policy_term_surrounding_whitespace] El término de política lingüística no puede tener espacios al principio o al final
    [language_policy_term_duplicate] El término de política lingüística no puede estar duplicado
    [language_id_blank] El identificador de idioma no puede estar vacío
    [language_id_surrounding_whitespace] El identificador de idioma no puede tener espacios al principio o al final
    [language_id_uses_underscore] El identificador de idioma debe separar las subetiquetas con guiones
    [language_id_invalid_syntax] El identificador de idioma debe cumplir la sintaxis RFC 5646
    [language_id_invalid_registry_tag] El identificador de idioma contiene una subetiqueta de registro no válida
    [language_id_canonicalization_failed] No se puede normalizar el identificador de idioma
    [language_id_undefined_primary_language] El identificador de idioma debe definir un idioma principal
    [language_id_duplicate] El identificador de idioma debe ser único
    [language_catalog_empty] Se requiere al menos un módulo de idioma de origen
    [url_invalid] El valor debe ser una URL válida
    [url_credentials_forbidden] La URL no puede contener credenciales
    [url_fragment_forbidden] La URL no puede contener un fragmento
    [url_scheme_unsupported] El esquema de URL debe ser http o https
    [api_key_blank] La API key no puede estar vacía
    [api_key_surrounding_whitespace] La API key no puede tener espacios al principio o al final
    [api_key_invalid_header] La API key no se puede representar como valor HTTP Header
    [strict_json_invalid] El valor debe ser JSON estricto (línea={ $line }, columna={ $column })
    [json_object_required] El valor debe ser un objeto JSON
    [reserved_request_field] El campo pertenece al protocolo de solicitud y no se puede sobrescribir
    [proxy_must_be_false_or_url] proxy debe ser false o una URL http/https completa
    [pem_path_duplicate] La ruta PEM debe ser única
    [runtime_maximum_exceeded] El valor supera el máximo de ejecución (valor real={ $actual }, máximo={ $maximum })
    [value_surrounding_whitespace] El valor no puede tener espacios al principio o al final
    [value_blank] El valor no puede estar vacío
    [path_blank] La ruta no puede estar vacía
    [positive_required] El valor debe ser mayor que cero (valor real={ $actual })
    [usize_range_exceeded] El valor supera el intervalo usize de esta plataforma (valor real={ $actual })
    [u32_range_exceeded] El valor supera el intervalo u32 (valor real={ $actual })
    [duplicate_profile_id] El identificador del perfil de traducción debe ser único
    [selected_profile_invalid] La estructura o los tipos de campo del perfil de traducción seleccionado no son válidos
    [referenced_client_not_found] El cliente LLM indicado no existe
   *[other] __ATT_FALLBACK__
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
    [thinking_empty] el contenido del razonamiento está vacío, línea { $line }, columna { $column }
   *[json] JSON de respuesta del modelo no válido (categoría `{ $category }`), línea { $line }, columna { $column }
}
task-record-attempt-succeeded = Intento { $number }: correcto; finish reason { $finish_reason }
task-record-attempt-token-usage = ; tokens `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ; duración `{ $duration }`
task-record-attempt-retryable = Intento { $number }: error reintentable; duración `{ $duration }`
task-record-attempt-retry-after = ; Retry-After `{ $duration }`
task-record-attempt-wait-retry = ; reintento tras `{ $duration }`
task-record-attempt-wait-completed = ; espera de `{ $duration }` completada; el siguiente intento no comenzó
task-record-attempt-wait-cancelled = ; espera prevista de `{ $duration }`; cancelado durante la espera
task-record-attempt-failed = Intento { $number }: error al procesar la solicitud o respuesta; duración `{ $duration }`
task-record-attempt-cancelled = Intento { $number }: cancelado; duración `{ $duration }`
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
task-record-task-diagnostic = Diagnóstico de tarea
task-record-duration-seconds = { $value } segundos
task-record-duration-milliseconds = { $value } ms
