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
    [process_startup] Inicio del proceso
    [process_output] Salida del proceso
    [configuration] Carga de la configuración
    [command_preparation] Preparación del comando
    [project_opening] Apertura del proyecto
    [init] Inicialización
    [extract] Extracción
    [translate] Traducción
    [write_back] Reescritura
    [lua] Ejecución Lua del proyecto
    [model_request] Solicitud al modelo
    [run_plan_finalization] Finalización del plan de ejecución
    [publication] Publicación
    [shutdown] Cierre
    [logging] Registro del proyecto
   *[other] __ATT_FALLBACK__
}
diagnostic-impact-value = { $code ->
    [unchanged] El estado no cambió
    [valid_progress_preserved] Se conservó el progreso válido
    [result_applied_but_run_plan_not_saved] El resultado se aplicó, pero el plan de ejecución no se guardó
    [state_applied_but_finalization_failed] El estado se aplicó, pero la finalización no terminó
    [recovery_required] Es necesario recuperar antes de confiar en el estado
    [outcome_unknown] Se desconoce el estado final
   *[other] __ATT_FALLBACK__
}
diagnostic-action-value = { $code ->
    [fix_configuration] Corrige el campo de configuración indicado y vuelve a intentarlo
    [fix_input] Corrige la entrada indicada y vuelve a intentarlo
    [check_path_and_permissions] Comprueba la ruta, el estado del sistema de archivos y los permisos
    [check_project_state] Revisa y corrige el estado del proyecto y vuelve a intentarlo
    [retry_after_resolving_contention] Espera a que termine la operación en conflicto y vuelve a intentarlo
    [check_model_service] Comprueba la respuesta del servicio de modelos y los límites de la cuenta
    [preserve_recovery_artifacts] No elimines los artefactos de recuperación indicados; recupera la salida antes de volver a intentarlo
    [retry] Vuelve a intentar la operación
    [report_bug] Informa de este defecto de ATT con el código de error y la ruta del registro
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] Falta un valor obligatorio
    [extract_plan_required] No hay un plan Extract reutilizable guardado; proporciona al menos una opción entre --builtin, --rules y --lua
    [conflicting_values] Los valores proporcionados son incompatibles
    [invalid_syntax] La sintaxis del valor no es válida
    [invalid_encoding] La codificación del texto no es válida
    [invalid_value] El valor incumple el contrato requerido
    [not_found] El objeto requerido no existe
    [busy] Otra operación está usando el recurso
    [state_mismatch] El estado guardado del proyecto no satisface esta operación
    [requirement_failed] No se cumple una condición previa obligatoria
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
    [lua_database_open_failed] El host Lua no pudo abrir la sesión de la base de datos del proyecto
    [lua_context_creation_failed] El entorno Lua no pudo crear el contexto de VM
    [lua_compilation_failed] No se pudo compilar el programa Lua principal
    [lua_execution_failed] El programa Lua principal falló durante la ejecución
    [lua_host_call_failed] Falló una llamada a una capacidad del host Lua
    [lua_finalization_failed] El host Lua no pudo finalizar todos los recursos vinculados
    [lua_unclosed_transaction] El programa Lua terminó con una transacción abierta; la transacción se revirtió
    [lua_snapshot_store_failed] No se pudo guardar la instantánea de extracción Lua validada
    [rules_definition_invalid] El programa Rules no cumple el contrato de definición de Rules
    [rules_document_read_failed] No se pudo leer un documento de origen requerido por Rules
    [rules_no_non_blank_match] La entrada Rules no produjo ninguna unidad semántica no vacía
    [rules_invalid_target] La entrada Rules seleccionó un valor que no puede usarse como destino de texto
    [rules_pattern_match_failed] No se pudo evaluar el patrón PCRE2 de Rules
    [rules_zero_width_match] El patrón Rules produjo una coincidencia de ancho cero
    [rules_overlapping_capture] El patrón Rules produjo capturas de texto superpuestas
    [rules_missing_text_capture] La captura de texto con nombre requerida no participó en la coincidencia
    [rules_invalid_capture_range] La coincidencia o captura de Rules está fuera de los límites válidos de caracteres UTF-8
    [rules_duplicate_target] Dos entradas Rules reclaman el mismo destino físico de texto
    [rules_invalid_materialization] La receta de proyección Rules no puede reconstruir el valor de origen
    [rules_snapshot_invalid] Los grupos Rules extraídos no forman una instantánea de recursos válida
    [rules_snapshot_store_failed] No se pudo guardar la instantánea de extracción Rules validada
    [write_back_extraction_out_of_date] Los recursos extraídos ya no coinciden con el origen actual del proyecto
    [write_back_asset_snapshot_invalid] Los recursos Standard guardados no forman una instantánea de reescritura válida
    [source_document_invalid] Un documento de origen de RPG Maker no cumple el formato requerido
    [write_back_mutation_invalid] No se puede aplicar una modificación de traducción validada en su ubicación de origen congelada
    [write_back_output_path_invalid] Un archivo reescrito está fuera del árbol de salida de RPG Maker permitido
    [write_back_output_path_duplicate] Más de un archivo reescrito apunta a la misma ruta de salida
    [write_back_candidate_project_mismatch] El candidato de reescritura preparado pertenece a otro proyecto
    [write_back_candidate_invalid] El candidato de reescritura no cumple la estructura de árbol data/js requerida
    [write_back_unexpected_lua_outcome] El programa Lua de reescritura devolvió un resultado para otra fase Lua
    [write_back_not_published] El candidato de reescritura no reemplazó el directorio de salida actual
    [write_back_published_with_residuals] La salida se publicó, pero no se pudieron eliminar algunos artefactos de recuperación
    [write_back_recovery_required] Hay que recuperar el directorio de salida antes de confiar en su contenido
    [internal_invariant] Se incumplió un invariante interno; se trata de un defecto de ATT
   *[other] __ATT_FALLBACK__
}
diagnostic-io-kind-value = { $code ->
    [not_found] No encontrado
    [permission_denied] Permiso denegado
    [connection_refused] Conexión rechazada
    [connection_reset] Conexión restablecida
    [host_unreachable] Host inaccesible
    [network_unreachable] Red inaccesible
    [connection_aborted] Conexión interrumpida
    [not_connected] Sin conexión
    [address_in_use] Dirección ya en uso
    [address_not_available] Dirección no disponible
    [network_down] Red fuera de servicio
    [broken_pipe] Canalización rota
    [already_exists] Ya existe
    [would_block] La operación se bloquearía
    [not_a_directory] No es un directorio
    [is_a_directory] Es un directorio
    [directory_not_empty] El directorio no está vacío
    [read_only_filesystem] Sistema de archivos de solo lectura
    [stale_network_file_handle] Identificador de archivo de red obsoleto
    [invalid_input] Entrada de operación no válida
    [invalid_data] Datos no válidos
    [timed_out] La operación agotó el tiempo de espera
    [write_zero] La escritura no progresó
    [storage_full] El almacenamiento está lleno
    [not_seekable] No se puede desplazar por el objeto
    [quota_exceeded] Cuota de almacenamiento superada
    [file_too_large] El archivo es demasiado grande para el sistema subyacente
    [resource_busy] Recurso ocupado
    [executable_file_busy] Archivo ejecutable ocupado
    [deadlock] La operación provocaría un interbloqueo
    [crosses_devices] La operación cruza dispositivos del sistema de archivos
    [too_many_links] Demasiados enlaces del sistema de archivos
    [invalid_filename] Nombre de archivo no válido
    [argument_list_too_long] La lista de argumentos del sistema operativo es demasiado larga
    [interrupted] Operación interrumpida
    [unsupported] Operación no compatible
    [unexpected_eof] Fin de archivo inesperado
    [out_of_memory] El sistema operativo no pudo asignar memoria
    [other] Otro error del sistema operativo
   *[unknown] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [runtime_configuration_invalid] La configuración de ejecución no es válida
    [unsupported_prompt_locale] Debe ser exactamente auto en minúsculas o una configuración regional de interfaz BCP 47 compatible
    [language_policy_term_blank] El término de política lingüística no puede estar vacío
    [language_policy_term_surrounding_whitespace] El término de política lingüística no puede tener espacios al principio o al final
    [language_policy_term_duplicate] El término de política lingüística no puede estar duplicado
    [quote_repair_candidates_empty] La lista de candidatos de reparación de comillas no puede estar vacía
    [quote_repair_delimiter_invalid] El delimitador de reparación de comillas no puede ser alfanumérico, espacio ni carácter de control
    [quote_repair_pair_duplicate] El par de reparación de comillas no puede estar duplicado
    [quote_repair_delimiter_ambiguous] El delimitador de reparación de comillas debe pertenecer exactamente a un par
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
diagnostic-io-reason = Operación { $operation }: { $kind }
diagnostic-io-reason-with-os-code = Operación { $operation }: { $kind } (SO { $os_code })
diagnostic-io-reason-with-system-message = Operación { $operation }: { $kind }: { $system_message }
diagnostic-io-reason-with-os-code-and-system-message = Operación { $operation }: { $kind } (SO { $os_code }): { $system_message }
diagnostic-failure-with-detail = { $failure }: { $detail }
diagnostic-invalid-utf8 = UTF-8 no válido en el byte { $valid_up_to }, longitud no válida de { $error_len } bytes
diagnostic-incomplete-utf8 = Secuencia UTF-8 incompleta después del byte { $valid_up_to }
diagnostic-toml-failure-value = { $code ->
    [syntax] La sintaxis TOML no es válida
    [missing_field] Falta un campo de configuración obligatorio
    [unknown_field] La configuración contiene un campo desconocido
    [duplicate_field] El campo de configuración se declaró más de una vez
    [type_mismatch] Se esperaba { $expected }
    [invalid_value] El valor de configuración incumple el contrato del campo
   *[other] __ATT_FALLBACK__
}
diagnostic-toml-expected-kind-value = { $code ->
    [string] una cadena
    [integer] un entero
    [boolean] un booleano
    [string_or_boolean] una cadena o un booleano
    [string_array] una matriz de cadenas
    [integer_array] una matriz de enteros
    [string_pair_array] una matriz de pares de cadenas
    [table] una tabla
    [table_array] una matriz de tablas
   *[other] __ATT_FALLBACK__
}
diagnostic-invalid-toml = TOML no válido ({ $resource }): { $failure }
diagnostic-invalid-toml-at = TOML no válido en la línea { $line }, columna { $column } ({ $resource }): { $failure }
diagnostic-http-no-details = La solicitud al servicio de modelos falló sin detalles públicos del estado HTTP
diagnostic-http-status = Estado HTTP { $status }
diagnostic-http-retry-after = Retry-After de { $seconds } segundos
diagnostic-http-provider-code = Código de error del proveedor { $code }
diagnostic-http-provider-type = Tipo de error del proveedor { $kind }
diagnostic-http-fact-separator = ;{ " " }
diagnostic-sqlite = Código de error principal de SQLite { $primary_code }, código ampliado { $extended_code }
diagnostic-windows-status = La operación de Windows { $operation } falló con NTSTATUS { $status }
diagnostic-resource = { $resource }: valor real { $actual }
diagnostic-resource-with-maximum = { $resource }: valor real { $actual }, máximo { $maximum }
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
    [tag_value_contains_closing_delimiter] La línea { $line } contiene '>' que cerraría el valor de la etiqueta prematuramente
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
