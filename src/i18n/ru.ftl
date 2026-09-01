app-about = Перевод игр и структурированного текста с повторно используемым состоянием проекта
cli-test-about = Проверить конфигурацию дистрибутива и все клиенты LLM
cli-ui-language-help = Язык справки, диагностики, прогресса, результатов и журналов проекта: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko или vi
cli-mz-about = Перевести игру RPG Maker MZ
cli-mv-about = Перевести игру RPG Maker MV
cli-generic-about = Перевести структурированный текст JSONL
cli-init-about = Инициализировать или обновить именованный проект перевода
cli-extract-about = Синхронизировать исходный текст из текущего входа проекта
cli-translate-about = Перевести извлечённый текст с явным или сохранённым Profile
cli-write-back-about = Записать текущие переводы в выходной каталог проекта
cli-manual-about = Управлять ручными переводами в редактируемом TOML-файле
cli-manual-export-about = Экспортировать строки, которым нужен ручной перевод
cli-ownership-export-about = Экспортировать владельца текста для каждой извлечённой единицы RPG Maker
cli-translation-export-about = Экспортировать исходный текст, текущий перевод и состояние каждой извлечённой единицы
cli-manual-check-about = Проверить TOML с ручными переводами без изменения проекта
cli-manual-apply-about = Применить заполненные и корректные ручные переводы
cli-project-lua-about = Выполнить Lua-скрипт над базой данных проекта
cli-project-name-help = Стабильное имя проекта
cli-init-path-help = Корневой входной каталог; существующий проект может повторно использовать последний успешный путь
cli-source-language-help = ID исходного языка
cli-target-language-help = ID целевого языка
cli-builtin-help = Использовать встроенные в ATT позиции текста RPG Maker
cli-rules-help = Заменить правила извлечения RPG Maker этим TOML; пустой список отключает правила
cli-dialogue-rules-help = Заменить проекцию имён диалога MV, используемую с Builtin
cli-profile-help = ID Profile перевода; при отсутствии используется последний успешный Profile
cli-terms-help = Заменить терминологический ресурс проекта
cli-placeholders-help = Заменить ресурс Placeholder проекта
cli-project-lua-script-help = Lua-скрипт для выполнения над базой данных проекта
cli-project-lua-arguments-help = Аргумент UTF-8 для Lua arg[1..] после --
cli-manual-file-help = TOML-файл с ручными переводами
cli-usage-heading = Использование:
cli-commands-heading = Команды:
cli-options-heading = Параметры:
cli-arguments-heading = Аргументы:
cli-options-metavar = ПАРАМЕТРЫ
cli-command-metavar = КОМАНДА
cli-print-help = Показать справку
cli-print-version = Показать версию
cli-blank-value = Значение не может быть пустым.
cli-invalid-positive-integer = Значение должно быть положительным целым числом.
cli-invalid-ui-language-argument = --ui-language содержит недопустимый языковой тег: { $value }.
cli-unsupported-ui-language-argument = --ui-language запрашивает неподдерживаемый язык: { $value }.
cli-invalid-ui-language-environment = ATT_UI_LANGUAGE содержит недопустимый языковой тег: { $value }.
cli-unsupported-ui-language-environment = ATT_UI_LANGUAGE запрашивает неподдерживаемый язык: { $value }.
cli-ui-language-environment-not-unicode = ATT_UI_LANGUAGE не является допустимым Unicode.
cli-unexpected-argument = Неожиданный аргумент: { $value }.
cli-missing-required-argument = Отсутствует обязательный аргумент: { $value }.
cli-invalid-value = Значение { $value } недопустимо для { $argument }.
cli-error-heading = Ошибка:
cli-try-help = Для дополнительной информации используйте --help.
cli-missing-value = Для { $argument } требуется значение.
cli-missing-subcommand = Необходимо указать команду.
cli-argument-conflict = { $argument } нельзя использовать с другими указанными аргументами.
cli-wrong-number-of-values = Для { $argument } указано неверное количество значений.
cli-invalid-utf8 = Аргумент командной строки не является допустимым Unicode.
cli-parse-failure = Не удалось разобрать командную строку.
plan-source-explicit = явный ввод
plan-source-project-state = состояние проекта
plan-source-product-default = поведение продукта
notice-init-reuse-path = Исходный путь не указан; используется последний успешный путь: { $path }.
notice-extract-reuse-owners = Область извлечения не указана; используется последний успешный план: { $owners }.
notice-translate-reuse-profile = Profile не указан; используется последний успешный Profile: { $profile }.
notice-no-model-request = Все единицы перевода актуальны; в этом запуске запрос к модели не требовался.
progress-init-check-project = Проверка состояния проекта
progress-init-scan-source = Сканирование исходников игры
progress-init-build-candidate = Построение кандидата проекта
progress-init-converge-database = Сведение базы данных проекта
progress-init-publish = Публикация инициализированного проекта
progress-save-run-plan = Сохранение успешного плана запуска
progress-extract-owner = Owner извлечения: { $owner }
progress-extract-documents = Сканирование документов
progress-extract-builtin = Единицы Builtin
progress-extract-rules = Определения Rules
progress-extract-commit = Commit извлечённых ресурсов
progress-generic-init = Инициализация проекта Generic
progress-generic-extract = Сканирование входных данных Generic JSONL
progress-translate-planning = Планирование задач перевода
progress-translate-confirmed = Подтверждено задач перевода
progress-no-work = Обработка не требуется
progress-project-lua = Выполнение программы Lua проекта
progress-write-back-read-assets = Чтение принятых ресурсов
progress-write-back-planning = Планирование перезаписи документов
progress-write-back-documents = Перезаписано документов
progress-write-back-validate-candidate = Проверка кандидата вывода
progress-write-back-publish = Публикация вывода; при прерывании ожидается подтверждённый результат
progress-finalizing = Завершение обязательных ресурсов
progress-safe-stopping = Безопасная остановка; последний подтверждённый прогресс сохранён
result-init-completed = Инициализация завершена: { $project }
result-init-created = Состояние проекта: создан
result-init-unchanged = Состояние проекта: без изменений
result-init-updated = Состояние проекта: обновлён
result-init-stale-owners = Требуется повторное извлечение: { $owners }
result-extract-completed = Извлечение завершено: { $project }
result-translate-completed = Запуск перевода завершён: { $project } (Profile: { $profile })
result-translate-status = Состояние: { $status }
result-translate-status-value = { $status ->
    [no_work] обработка не требуется
    [complete] полностью
    [incomplete] не полностью
   *[other] __ATT_FALLBACK__
}
result-translate-summary = Перевод: запланировано { $total } задач, начато { $started }, не начато { $not_started }; завершено { $complete }, частично { $partial }, недоступно { $unavailable }, с ошибкой { $failed }, отменено { $cancelled }; записано { $written } позиций, осталось { $remaining }, из них отклонено { $rejected }
result-translate-convergence = Сведение состояния: сохранено { $retained }, аннулировано { $invalidated }, неприменимо { $not_applicable }, переиспользовано { $reused }
result-write-back-completed = Запись завершена: { $project }
result-project-lua-completed = Выполнение Lua проекта завершено: { $project }
result-output-directory = Каталог вывода: { $path }
result-write-back-summary = Запись: { $translated } переведённых единиц, { $original } исходных
result-generic-extract-unchanged = Входные данные Generic не изменились: файлов — { $files }, групп — { $groups }, единиц — { $units }
result-generic-extract-updated = Входные данные Generic обновлены: файлов — { $files }, групп — { $groups }, единиц — { $units }; переводов сохранено — { $preserved }, очищено — { $cleared }
result-generic-translate-summary = Перевод Generic: запланировано { $total } задач, начато { $started }, не начато { $not_started }; завершено { $complete }, частично { $partial }, недоступно { $unavailable }, с ошибкой { $failed }, отменено { $cancelled }; запланировано Unit: { $planned_units }, осталось Unit: { $remaining_units }, из них отклонено { $rejected_units }, очищено { $cleared }, повторно использовано { $reused }, принято { $accepted }, записано { $written }, конфликтов { $conflicted }, проблем ответа { $problems }
result-generic-write-back-summary = Запись Generic: { $translated } переведённых единиц, { $original } исходных сохранено
result-run-log = Журнал запуска: { $path }
result-test-configuration = Конфигурация: { $status ->
    [passed] успешно
   *[failed] ошибка
}
result-test-client = LLM { $client }: { $status ->
    [passed] успешно
   *[failed] ошибка
} ({ $protocol }, { $stream ->
    [streaming] потоковый режим
   *[non_streaming] полный ответ
})
result-test-summary = Итог: { $passed }/{ $total } успешно, { $failed } с ошибкой, { $skipped } не запущено
translate-incomplete-object = Запуск Translate для проекта { $project }
translate-incomplete-rpg-maker-reason = Частичных задач: { $partial }, недоступных: { $unavailable }, не начато: { $not_started }, проблем протокола: { $protocol }, исчерпанных запросов: { $exhausted }; приём запросов {
    $admission ->
        [stopped] остановлен
       *[open] продолжен
    }; осталось решений: { $remaining_decisions }, мест: { $remaining_locations }, из них отклонено: { $rejected_locations }
translate-incomplete-generic-reason = Частичных задач: { $partial }, недоступных: { $unavailable }, не начато: { $not_started }, исчерпанных запросов: { $exhausted }; приём запросов {
    $admission ->
        [stopped] остановлен
       *[open] продолжен
    }; осталось Unit: { $remaining_units }, из них отклонено: { $rejected_units }, конфликтов записи: { $conflicted }, проблем ответа: { $problems }
translate-incomplete-help = Изучите диагностику задач в журнале этого запуска, исправьте повторяемые проблемы и снова запустите Translate; для небольшого остатка используйте Manual
translate-incomplete-rejected-help = Изучите диагностику задач; повторите отклонённый перевод с --retry-rejected или экспортируйте его командой manual export --selection rejected для обработки через Manual
result-cancelled = Команда отменена после безопасного завершения.
result-plan-saved = Успешный план запуска сохранён.
log-run-started = Команда { $command } запущена.
log-run-succeeded = Команда { $command } успешно завершена.
log-run-failed = Команда { $command } завершилась ошибкой.
log-run-outcome-unknown = Команда { $command } завершилась, но итоговое состояние неизвестно; выполните указания диагностики перед повторной попыткой.
log-run-cancelled = Команда { $command } отменена.
log-performance-counters = Счётчики производительности: попыток управления транзакциями SQLite — { $sqlite_control_attempted_total }; полных проверок дерева-кандидата начато — { $candidate_validation_started }, завершено — { $candidate_validation_completed }.
log-lua-print = Lua: { $message }
log-plan-resolved = План команды { $command } получен из { $source }.
log-phase-started = Этап начат: { $phase }.
log-retry-summary = { $count ->
    [one] Выполнен { $count } повтор.
    [few] Выполнено { $count } повтора.
    [many] Выполнено { $count } повторов.
   *[other] Выполнено { $count } повтора.
}
log-translation-task-started = Задача перевода { $index }/{ $total } запущена.
log-translation-task-finished = Задача перевода { $index } завершена с результатом { $outcome }.
log-run-recovery-required = Команда { $command } завершилась в состоянии, требующем восстановления; используйте пути из диагностики.
log-phase-completed = Этап завершён: { $phase }.
log-phase-stopped = { $outcome ->
    [failed] Этап завершился ошибкой: { $phase }.
    [cancelled] Этап отменён: { $phase }.
   *[other] Этап остановлен: { $phase }.
}
log-cancellation-requested = Запрошена отмена после подтверждения { $confirmed } из { $total } элементов.
log-cancellation-requested-indeterminate = Запрошена отмена после подтверждения { $confirmed } элементов; общее число неизвестно.
log-run-plan-finalized = { $result ->
    [saved] План запуска сохранён.
    [not_saved] План запуска не сохранён.
    [saved_finalization_failed] План запуска сохранён, но завершение обработки не удалось.
    [outcome_unknown] Итоговое состояние плана запуска неизвестно.
   *[other] Обработка плана остановилась без распознанного результата.
}
log-translation-finished = { $result ->
    [not_started] Перевод не начался.
    [no_work] Перевод завершён, работа не требовалась.
    [complete] Перевод завершён.
    [incomplete] Перевод завершён не полностью.
    [failed] Перевод завершился ошибкой.
    [cancelled] Перевод отменён.
   *[other] Перевод остановился без распознанного результата.
}
log-publication-started = Начата публикация в корневой каталог вывода { $path }.
log-publication-finished = { $result ->
    [published] Публикация завершена.
    [not_published] Публикация не изменила вывод.
    [recovery_required] Публикация остановлена и требует восстановления.
    [outcome_unknown] Итоговое состояние публикации неизвестно.
   *[other] Публикация остановилась без распознанного результата.
}
log-task-outcome-value = { $outcome ->
    [complete] завершена
    [partial] завершена частично
    [unavailable] недоступна
    [failed] завершилась ошибкой
    [not_committed_after_earlier_failure] не зафиксирована после предыдущей ошибки
    [cancelled] отменена
   *[other] завершилась без распознанного результата
}
diagnostic-object = Объект: { $subject }
diagnostic-error-heading = Ошибка:
diagnostic-warning-heading = Предупреждение:
diagnostic-explanation = Причина: { $reason }
diagnostic-impact = Влияние: { $impact }
diagnostic-resolution = Действие: { $action }
diagnostic-related = { $relation ->
    [cleanup] Также не удалось выполнить очистку:
    [rollback] Также не удалось выполнить откат:
    [discard] Также не удалось удалить кандидат:
    [finalization] Также не удалось завершить обязательные действия:
    [shutdown] Также не удалось выполнить остановку:
    [observability] Также не удалось показать или записать результат:
   *[other] Также произошёл сбой связанной операции:
}
diagnostic-impact-value = { $effect ->
    [unchanged] Рабочее состояние не изменено
    [progress_preserved] Ранее подтверждённый прогресс сохранён; указанное содержимое не завершено
    [applied] Связанный рабочий результат уже применён
    [applied_run_plan_not_saved] Рабочий результат применён, но план этого запуска не сохранён
    [applied_finalization_failed] Рабочий результат применён, но обязательное завершение не выполнено
    [recovery_required] Результат известен, но сначала нужно обработать указанное место восстановления
    [outcome_unknown] Нельзя подтвердить, применена ли операция; до выполнения указанного действия не повторяйте её и не удаляйте материалы восстановления
   *[other] __ATT_FALLBACK__
}
diagnostic-resolution-value = { $code ->
    [fix_configuration] Исправьте указанный параметр конфигурации и повторите попытку
    [fix_input] Исправьте указанные входные данные и повторите попытку
    [fix_placeholder_rules] Исправьте указанное правило Placeholder и повторите попытку
    [review_translation] Проверьте указанный перевод; при необходимости исправьте его через Manual
    [review_disabled_rules] Если это ожидаемый результат, ничего делать не нужно; иначе добавьте допустимые правила в указанный файл и снова запустите Extract
    [check_path_and_permissions] Проверьте путь, состояние файловой системы и разрешения
    [check_project_state] Проверьте и исправьте состояние проекта, затем повторите попытку
    [resolve_contention] Дождитесь завершения конфликтующей операции и повторите попытку
    [check_model_service] Проверьте ответ службы модели и ограничения учётной записи
    [preserve_recovery_artifacts] Не удаляйте указанные артефакты восстановления; восстановите вывод перед повторной попыткой
    [retry] Повторите операцию
    [report_bug] Сообщите об этой ошибке ATT и опишите выполнявшуюся операцию
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] Отсутствует обязательное значение
    [generic_extract_required] Входные JSONL больше не соответствуют последнему Extract; снова выполните att generic extract
    [conflicting_values] Указанные значения конфликтуют
    [invalid_syntax] Значение имеет недопустимый синтаксис
    [invalid_encoding] Недопустимая кодировка текста
    [invalid_value] Значение нарушает обязательный контракт
    [empty_text_capture] Именованная группа text пуста
    [rules_owner_disabled] Выбранный файл Rules содержит rule = []; Rules отключён, а извлечённые им ресурсы удалены
    [not_found] Требуемый объект не существует
    [state_mismatch] Сохранённое состояние проекта не соответствует этой операции
    [unsupported_windows_code_page] Кодовая страница Windows не UTF-8
    [transaction_rolled_back] Транзакция завершилась ошибкой, изменения отменены
    [transaction_outcome_unknown] Транзакция завершилась без подтверждения фиксации или отката
    [finalization_failed] Результат операции существует, но завершение не удалось
    [rollback_failed] Основная операция и откат завершились ошибкой
    [external_service_rejected] Внешняя служба отклонила запрос
    [external_service_unavailable] Внешняя служба недоступна
    [executor_closed] Служба выполнения завершается или уже закрыта
    [concurrent_shutdown] Другой вызывающий уже завершает исполнитель
    [executor_state_poisoned] Состояние жизненного цикла исполнителя повреждено
    [worker_spawn_failed] Операционная система не смогла создать рабочий поток
    [stdout_write_failed] Не удалось записать в стандартный вывод
    [stderr_write_failed] Не удалось записать в стандартный поток ошибок
    [stdout_flush_failed] Не удалось сбросить стандартный вывод
    [stderr_flush_failed] Не удалось сбросить стандартный поток ошибок
    [worker_channel_closed] Канал команд рабочего потока закрылся до завершения финализации
    [worker_panicked] Рабочий поток неожиданно завершился
    [reparse_point_forbidden] Путь содержит недоверенную точку повторного анализа
    [non_local_volume] Путь находится не на локальном фиксированном томе
    [non_ntfs_volume] Путь находится не на томе NTFS
    [case_sensitive_directory] Каталог использует имена с учётом регистра
    [lock_cancelled] Ожидание требуемой блокировки отменено
    [target_already_exists] Назначение уже существует
    [file_identity_changed] Идентификатор файла изменился во время операции
    [invalid_path] Путь не является допустимой целью этой операции
    [not_regular_file] Существующий объект не является обычным файлом
    [wrong_publisher_instance] Токен публикации принадлежит другому экземпляру издателя
    [journal_corrupt] Журнал восстановления публикации недействителен или неполон
    [unexpected_artifact] Неожиданный артефакт файловой системы блокирует операцию
    [interactive_session_already_open] Другой интерактивный сеанс SQLite уже активен
    [backup_incomplete] Резервное копирование SQLite не достигло состояния завершения
    [request_serialization_failed] Не удалось сериализовать запрос к модели
    [http_client_build_failed] Не удалось создать HTTP-клиент сервиса моделей
    [dns_resolution_failed] Не удалось разрешить DNS-имя
    [tcp_connection_failed] Не удалось установить TCP-соединение
    [request_send_failed] Не удалось отправить HTTP-запрос
    [response_read_failed] Не удалось прочитать HTTP-ответ
    [tls_handshake_failed] Ошибка согласования TLS
    [connect_timed_out] Истекло время ожидания TCP-соединения
    [read_timed_out] Истекло время чтения HTTP-ответа
    [request_timed_out] HTTP-запрос превысил общий тайм-аут
    [response_decode_failed] Не удалось декодировать HTTP-ответ
    [redirect_rejected] HTTP-перенаправление отклонено
    [response_parsing_failed] Ответ модели не является допустимым JSON
    [model_stream_invalid_json] Событие потока модели не является допустимым JSON
    [model_stream_invalid_utf8] Поток модели содержит недопустимый UTF-8
    [model_stream_error_event] Поток модели вернул событие ошибки сервиса
    [model_stream_unclosed_event] Событие SSE не закрыто пустой строкой
    [model_stream_missing_finish] В потоке Chat отсутствует finish_reason
    [model_stream_missing_responses_terminal] В потоке Responses отсутствует конечное событие
    [model_stream_event_type_mismatch] Имя события SSE не совпадает с типом JSON
    [model_stream_duplicate_choice] Поток модели повторил один и тот же choice
    [model_stream_output_after_finish] Поток модели продолжил вывод после finish
    [model_stream_unexpected_done] Поток Responses вернул неожиданный [DONE]
    [invalid_response_contract] Ответ модели не соответствует обязательному контракту
    [needs_review] Перевод требует проверки
    [lua_compilation_failed] Не удалось скомпилировать основную программу Lua
    [lua_execution_failed] Ошибка во время выполнения основной программы Lua
    [rules_pattern_match_failed] Не удалось вычислить шаблон PCRE2 Rules
    [rules_zero_width_match] Шаблон Rules создал совпадение нулевой ширины
    [rules_overlapping_capture] Шаблон Rules создал перекрывающиеся текстовые захваты
    [rules_missing_text_capture] Обязательный именованный захват текста не участвовал в совпадении
    [rules_invalid_capture_range] Совпадение или захват Rules находится вне допустимых границ символов UTF-8
    [write_back_candidate_invalid] Кандидат обратной записи не соответствует требуемой структуре дерева data/js
    [write_back_recovery_required] Перед использованием содержимого каталога вывода требуется восстановление
    [already_exists] Целевой объект уже существует
    [cancelled] Операция отменена
    [concurrent_modification] Состояние проекта было изменено параллельно
    [duplicate_identifier] Идентификатор повторяется
    [extraction_out_of_date] Сохранённые данные извлечения больше не соответствуют текущему источнику
    [invalid_content] Содержимое нарушает обязательный контракт
    [operation_failed] Операция завершилась ошибкой
    [placeholder_projection_failed] Проекция Placeholder не сохранила обязательную структуру
    [profile_not_found] Выбранный профиль перевода не существует
    [recovery_required] Прежде чем считать результат достоверным, необходимо восстановление
    [resource_limit] Достигнут требуемый предел ресурса
    [resource_limit_exceeded] Операция превысила ограничение ресурса сервиса
    [source_snapshot_mismatch] Источник больше не соответствует сохранённому снимку
    [unavailable] Запрошенная работа временно недоступна
    [internal_invariant] Нарушен внутренний инвариант; это дефект ATT
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] Термин языковой политики не должен быть пустым
    [language_policy_term_surrounding_whitespace] Термин языковой политики не должен содержать пробелы по краям
    [language_policy_term_duplicate] Термин языковой политики не должен повторяться
    [language_id_blank] Идентификатор языка не должен быть пустым
    [language_id_surrounding_whitespace] Идентификатор языка не должен содержать пробелы по краям
    [language_id_uses_underscore] Идентификатор языка должен разделять подтеги дефисами
    [language_id_invalid_syntax] Идентификатор языка должен соответствовать синтаксису RFC 5646
    [language_id_invalid_registry_tag] Идентификатор языка содержит недопустимый подтег реестра
    [language_id_canonicalization_failed] Не удалось канонизировать идентификатор языка
    [language_id_undefined_primary_language] Идентификатор языка должен задавать основной язык
    [language_id_duplicate] Идентификатор языка должен быть уникальным
    [language_catalog_empty] Требуется хотя бы один модуль исходного языка
    [url_invalid] Значение должно быть допустимым URL
    [url_credentials_forbidden] URL не должен содержать учётные данные
    [url_fragment_forbidden] URL не должен содержать фрагмент
    [url_scheme_unsupported] Схема URL должна быть http или https
    [api_key_blank] API key не должен быть пустым
    [api_key_surrounding_whitespace] API key не должен содержать пробелы по краям
    [api_key_invalid_header] API key нельзя представить как значение HTTP Header
    [strict_json_invalid] Значение должно быть строгим JSON (строка={ $line }, столбец={ $column })
    [json_object_required] Значение должно быть объектом JSON
    [reserved_request_field] Поле принадлежит протоколу запроса и не может быть переопределено
    [proxy_must_be_false_or_url] proxy должен быть false или полным URL http/https
    [pem_path_duplicate] Путь PEM должен быть уникальным
    [runtime_maximum_exceeded] Значение превышает максимум среды выполнения (фактическое={ $actual }, максимум={ $maximum })
    [value_surrounding_whitespace] Значение не должно содержать пробелы по краям
    [value_blank] Значение не должно быть пустым
    [path_blank] Путь не должен быть пустым
    [positive_required] Значение должно быть больше нуля (фактическое={ $actual })
    [usize_range_exceeded] Значение превышает диапазон usize этой платформы (фактическое={ $actual })
    [u32_range_exceeded] Значение превышает диапазон u32 (фактическое={ $actual })
    [duplicate_profile_id] Идентификатор профиля перевода должен быть уникальным
    [selected_profile_invalid] Структура или типы полей выбранного профиля перевода недействительны
    [referenced_client_not_found] Указанный клиент LLM не существует
   *[other] __ATT_FALLBACK__
}
diagnostic-http-status = Статус HTTP { $status }
diagnostic-http-route-direct = Прямое соединение (без прокси)
diagnostic-http-route-proxy = Через явно заданный прокси { $proxy }
diagnostic-retry-after = Retry-After: { $seconds } с
diagnostic-provider-code = Код провайдера: { $code }
diagnostic-provider-type = Тип провайдера: { $kind }
diagnostic-provider-message = Сообщение провайдера: { $message }
diagnostic-json-position = строка { $line }, столбец { $column }
diagnostic-placeholder-rule-file = Правило Placeholder { $number } в { $path }
diagnostic-placeholder-rule-project = Правило Placeholder { $number } текущего проекта
manual-exported = Экспортировано записей: { $entries }; файл: { $path }
manual-checked = Допустимых: { $valid }, незаполненных: { $unfilled }, ошибок: { $errors }
manual-applied = Применено: { $applied }, незаполненных: { $unfilled }, ошибок: { $errors }
manual-value = { $code ->
    [invalid_source_line] элемент source { $line } содержит перевод строки или NUL
    [invalid_translation_line] элемент translation { $line } содержит перевод строки или NUL
    [fixed_length] для перевода fixed требуется элементов: { $expected }; получено: { $actual }
    [fixed_blank_slot] элемент { $line } перевода fixed должен оставаться пустым
    [rerun_export] Снова выполните manual export
    [rerun_export_without_controls] Снова выполните manual export и не добавляйте переводы строк или NUL в элементы массива
    [rerun_export_then_fill] Снова выполните manual export, затем заполните перевод
    [resolve_temporary_then_rerun_export] Исправьте указанный фиксированный временный путь, удалите оставшийся объект и снова выполните manual export
    [resolve_published_backup_cleanup] Оба файла уже опубликованы; проверьте их и удалите указанный фиксированный файл backup
    [keep_exported_type] Сохраните type, записанный командой manual export
   *[other] __ATT_FALLBACK__
}
task-record-title = Задача перевода
task-record-final-result-heading = Итоговый результат
task-record-final-status = Состояние: { $state ->
    [complete] завершена, фиксация подтверждена
    [partial] частично завершена, фиксация подтверждена
    [unavailable] недоступна, проект не изменён
    [execution_failed] ошибка выполнения, без фиксации
    [commit_preparation_failed] ошибка подготовки фиксации, точно не применена
    [commit_not_applied] транзакция точно не применена
    [commit_outcome_unknown] результат фиксации неизвестен
    [not_committed_after_earlier_failure] не зафиксирована из-за ошибки предыдущей задачи
    [invalid_result] недопустимая последовательность результатов Executor, без фиксации
    [cancelled] отменена, без фиксации
   *[other] { $state }
}
task-record-requested = Запрошено переводов: { $requested }
task-record-accepted-written = Принято: { $accepted } элементов (ID: { $ids }), записано в { $written } фактических позиций
task-record-accepted-outcome-unknown = Проверено: { $accepted } элементов (ID: { $ids }); результат фиксации базы данных невозможно подтвердить
task-record-unaccepted = Не принято: { $unaccepted } элементов (ID: { $ids })
task-record-task-diagnostic = Диагностика задачи
