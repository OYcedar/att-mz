app-about = Перевод игр и структурированного текста с повторно используемым состоянием проекта
cli-ui-language-help = Язык справки, диагностики, прогресса, результатов и журналов проекта: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko или vi
cli-progress-help = Режим текущего прогресса: auto, plain или off
cli-mz-about = Перевести игру RPG Maker MZ
cli-mv-about = Перевести игру RPG Maker MV
cli-generic-about = Перевести структурированный текст JSONL
cli-init-about = Инициализировать или обновить именованный проект перевода
cli-extract-about = Синхронизировать исходный текст из текущего входа проекта
cli-translate-about = Перевести извлечённый текст с явным или сохранённым Profile
cli-write-back-about = Записать текущие переводы в выходной каталог проекта
cli-project-lua-about = Однократно выполнить атомарный Lua для базы данных проекта
cli-project-name-help = Стабильное имя проекта
cli-init-path-help = Корневой входной каталог; существующий проект может повторно использовать последний успешный путь
cli-source-language-help = ID исходного языка
cli-target-language-help = ID целевого языка
cli-dialogue-width-help = Максимум полноширинных символов в строке диалога
cli-scrolling-width-help = Максимум полноширинных символов в строке прокручиваемого текста
cli-help-width-help = Максимум полноширинных символов в строке справки или описания
cli-builtin-help = Использовать встроенные в ATT позиции текста RPG Maker
cli-rules-help = Заменить правила извлечения RPG Maker этим TOML; пустой список отключает правила
cli-dialogue-rules-help = Заменить проекцию имён диалога MV, используемую с Builtin
cli-profile-help = ID Profile перевода; при отсутствии используется последний успешный Profile
cli-terms-help = Заменить терминологический ресурс проекта
cli-placeholders-help = Заменить ресурс Placeholder проекта
cli-project-lua-script-help = Атомарная программа Lua для базы данных, выполняемая один раз
cli-project-lua-arguments-help = Аргумент UTF-8 для Lua arg[1..] после --
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
cli-invalid-progress = Режим прогресса { $value } не поддерживается; используйте auto, plain или off.
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
log-label-phase-check-project = проверка проекта
log-label-phase-scan-source = сканирование источника
log-label-phase-prepare-candidate = подготовка кандидата
log-label-phase-update-database = обновление базы данных
log-label-phase-publish = публикация
log-label-phase-builtin = встроенное извлечение
log-label-phase-rules = извлечение по правилам
log-label-phase-lua = обработка Lua
log-label-phase-planning = планирование
log-label-phase-confirmed-tasks = подтверждение задач
log-label-phase-no-work = работа не требуется
log-label-phase-read-assets = чтение ресурсов
log-label-phase-plan-rpg-maker-write-back = планирование записи RPG Maker
log-label-phase-rewrite-documents = перезапись документов
log-label-phase-validate-candidate = проверка кандидата
log-label-task-complete = полностью
log-label-task-partial = частично
log-label-task-unavailable = недоступно
log-label-task-failed = ошибка
error-state-applied-finalization = Результат применён, но завершение не удалось. Перед повтором проверьте состояние проекта.
error-no-executable-extract-owner = После очистки не осталось исполняемых owner Extract, поэтому план не сохранён.
error-plan-save-failed-applied = Результат команды применён, но новый план запуска не сохранён. В следующий раз явно укажите нужные параметры.
error-plan-save-outcome-unknown = Результат команды применён, но commit плана запуска нельзя подтвердить. В следующий раз явно укажите нужные параметры.
plan-source-explicit = явный ввод
plan-source-project-state = состояние проекта
plan-source-product-default = поведение продукта
notice-init-reuse-path = Исходный путь не указан; используется последний успешный путь: { $path }.
notice-extract-reuse-owners = Область извлечения не указана; используется последний успешный план: { $owners }.
notice-translate-reuse-profile = Profile не указан; используется последний успешный Profile: { $profile }.
notice-owner-disabled = Owner { $owner } отключён и удалён из будущих автоматических планов.
warning-rules-command-non-string-skipped = Предупреждение: правило Rules { $rule_number } пропустило нестроковые параметры command: { $skipped_count } (источник { $source_file }, code={ $command_code }, parameter={ $parameter }, тип { $actual_type }).
warning-manual-layout-required = Предупреждение: проверьте переносы строк вручную для { $locations } (region={ $region }, max_fullwidth_chars={ $max_fullwidth_chars }).
notice-no-model-request = Все единицы перевода актуальны; в этом запуске запрос к модели не требовался.
notice-manual-layout = { $count ->
    [one] 1 единица требует ручной проверки переноса строк.
    [few] { $count } единицы требуют ручной проверки переноса строк.
    [many] { $count } единиц требуют ручной проверки переноса строк.
   *[other] { $count } единицы требуют ручной проверки переноса строк.
}
notice-log-degraded = Журнал проекта недоступен или работает с ошибками; команда продолжится, а код выхода не изменится.
notice-task-records-degraded = Записи задач перевода недоступны или создаются с ошибками; команда продолжится, а код выхода не изменится.
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
progress-translate-no-work = Запрос к модели не нужен
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
result-translate-completed = Перевод завершён: { $project } (Profile: { $profile })
result-translate-summary = Перевод: { $total } задач; завершено { $complete }, частично { $partial }, недоступно { $unavailable }; записано { $written } позиций, осталось { $remaining }
result-translate-convergence = Сведение состояния: сохранено { $retained }, аннулировано { $invalidated }, неприменимо { $not_applicable }, переиспользовано { $reused }
result-write-back-completed = Запись завершена: { $project }
result-project-lua-completed = Выполнение Lua проекта завершено: { $project }
result-output-directory = Каталог вывода: { $path }
result-write-back-summary = Запись: { $translated } переведённых единиц, { $original } исходных; автоперенос { $auto_wrapped }, добавлено переносов { $breaks } и полноширинных отступов { $indents }; ручная раскладка { $manual }
result-generic-extract-unchanged = Входные данные Generic не изменились: файлов — { $files }, групп — { $groups }, единиц — { $units }
result-generic-extract-updated = Входные данные Generic обновлены: файлов — { $files }, групп — { $groups }, единиц — { $units }; переводов сохранено — { $preserved }, очищено — { $cleared }
result-generic-translate-summary = Перевод Generic: { $total } задач; завершено { $complete }, частично { $partial }, недоступно { $unavailable }; очищено { $cleared }, повторно использовано { $reused }, принято { $accepted }, записано { $written }, конфликтов { $conflicted }, проблем ответа { $problems }
result-generic-write-back-summary = Запись Generic: { $translated } переведённых единиц, { $original } исходных сохранено
result-cancelled = Команда отменена после безопасного завершения.
result-plan-saved = Успешный план запуска сохранён.
log-run-started = Команда { $command } запущена.
log-run-succeeded = Команда { $command } успешно завершена.
log-run-failed = Команда { $command } завершилась ошибкой.
log-run-outcome-unknown = Команда { $command } завершилась, но итоговое состояние неизвестно; используйте пути восстановления из ошибки.
log-run-cancelled = Команда { $command } отменена.
log-performance-counters = Счётчики производительности: попыток управления транзакциями SQLite — { $sqlite_control_attempted_total }; полных проверок дерева-кандидата начато — { $candidate_validation_started }, завершено — { $candidate_validation_completed }.
log-lua-script = Сценарий Lua { $identity } (SHA-256 { $fingerprint }).
log-lua-print = Lua: { $message }
log-lua-summary = Статистика Lua: вызовов базы данных — { $database_calls }, изменено строк — { $changed_rows }, вызовов перевода — { $translation_calls }, строк print — { $printed_lines }.
log-plan-resolved = План команды { $command } получен из { $source }.
log-phase-started = Этап начат: { $phase }.
log-phase-finished = Этап завершён: { $phase }.
log-retry-summary = { $count ->
    [one] Выполнен { $count } повтор.
    [few] Выполнено { $count } повтора.
    [many] Выполнено { $count } повторов.
   *[other] Выполнено { $count } повтора.
}
log-no-work = Работа не потребовалась: { $reason }.
log-no-work-translation-up-to-date = переводы уже соответствуют текущему источнику и профилю
log-partial-result = { $count ->
    [one] { $count } частичный результат требует внимания.
    [few] { $count } частичных результата требуют внимания.
    [many] { $count } частичных результатов требуют внимания.
   *[other] { $count } частичного результата требуют внимания.
}
log-translation-task-started = Задача перевода { $index }/{ $total } запущена.
log-translation-task-finished = Задача перевода { $index } завершена с результатом { $outcome }.
log-translation-task-diagnostic = Задача перевода { $index } сообщила диагностику после { $attempts } попыток: { $diagnostic }
diagnostic-title = Ошибка [{ $code }]
diagnostic-stage = Этап: { $stage }
diagnostic-subject = Место: { $subject }
diagnostic-subject-value = { $kind ->
    [command] команда { $value }
    [field] поле { $value }
    [project] проект { $value }
    [profile] профиль { $value }
    [component] компонент { $value }
   *[other] { $value }
}
diagnostic-reason = Причина: { $reason }
diagnostic-impact = Последствия: { $impact }
diagnostic-action = Действие: { $action }
diagnostic-recovery = Восстановление: { $recovery }
diagnostic-recovery-value = { $kind ->
    [component] компонент { $value }
    [transaction] транзакция { $value }
   *[other] { $value }
}
diagnostic-related = Связанная ошибка { $index }:
diagnostic-stage-value = { $code ->
    [process_startup] Запуск процесса
    [process_output] Вывод процесса
    [configuration] Загрузка конфигурации
    [command_preparation] Подготовка команды
    [project_opening] Открытие проекта
    [init] Инициализация
    [extract] Извлечение
    [translate] Перевод
    [write_back] Обратная запись
    [lua] Выполнение Lua проекта
    [model_request] Запрос к модели
    [run_plan_finalization] Завершение плана запуска
    [publication] Публикация
    [shutdown] Завершение работы
    [logging] Журнал проекта
   *[other] __ATT_FALLBACK__
}
diagnostic-impact-value = { $code ->
    [unchanged] Состояние не изменилось
    [valid_progress_preserved] Допустимый прогресс сохранён
    [result_applied_but_run_plan_not_saved] Результат применён, но план запуска не сохранён
    [state_applied_but_finalization_failed] Состояние применено, но завершение не выполнено
    [recovery_required] Прежде чем доверять состоянию, требуется восстановление
    [outcome_unknown] Итоговое состояние неизвестно
   *[other] __ATT_FALLBACK__
}
diagnostic-action-value = { $code ->
    [fix_configuration] Исправьте указанный параметр конфигурации и повторите попытку
    [fix_input] Исправьте указанные входные данные и повторите попытку
    [check_path_and_permissions] Проверьте путь, состояние файловой системы и разрешения
    [check_project_state] Проверьте и исправьте состояние проекта, затем повторите попытку
    [retry_after_resolving_contention] Дождитесь завершения конфликтующей операции и повторите попытку
    [check_model_service] Проверьте ответ службы модели и ограничения учётной записи
    [preserve_recovery_artifacts] Не удаляйте указанные артефакты восстановления; восстановите вывод перед повторной попыткой
    [retry] Повторите операцию
    [report_bug] Сообщите об этой ошибке ATT, указав код ошибки и путь к журналу
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] Отсутствует обязательное значение
    [extract_plan_required] Нет сохранённого плана Extract для повторного использования; укажите --builtin или --rules
    [generic_extract_required] Входные JSONL больше не соответствуют последнему Extract; снова выполните att generic extract
    [conflicting_values] Указанные значения конфликтуют
    [invalid_syntax] Значение имеет недопустимый синтаксис
    [invalid_encoding] Недопустимая кодировка текста
    [invalid_value] Значение нарушает обязательный контракт
    [not_found] Требуемый объект не существует
    [busy] Ресурс занят другой операцией
    [state_mismatch] Сохранённое состояние проекта не соответствует этой операции
    [requirement_failed] Обязательное предварительное условие не выполнено
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
    [wrong_publisher_instance] Токен публикации принадлежит другому экземпляру издателя
    [journal_corrupt] Журнал восстановления публикации недействителен или неполон
    [unexpected_artifact] Неожиданный артефакт файловой системы блокирует операцию
    [interactive_session_already_open] Другой интерактивный сеанс SQLite уже активен
    [backup_incomplete] Резервное копирование SQLite не достигло состояния завершения
    [request_serialization_failed] Не удалось сериализовать запрос к модели
    [response_parsing_failed] Ответ модели не является допустимым JSON
    [invalid_response_contract] Ответ модели не соответствует обязательному контракту
    [transport_failed] Перед получением допустимого ответа произошла ошибка транспорта HTTP
    [lua_database_open_failed] Хост Lua не смог открыть сеанс базы данных проекта
    [lua_context_creation_failed] Среда Lua не смогла создать контекст VM
    [lua_compilation_failed] Не удалось скомпилировать основную программу Lua
    [lua_execution_failed] Ошибка во время выполнения основной программы Lua
    [lua_host_call_failed] Ошибка вызова возможности хоста Lua
    [lua_finalization_failed] Хост Lua не смог завершить все связанные ресурсы
    [rules_definition_invalid] Программа Rules не соответствует контракту определения Rules
    [rules_document_read_failed] Не удалось прочитать исходный документ, требуемый программой Rules
    [rules_no_non_blank_match] Запись Rules не создала непустую семантическую единицу
    [rules_invalid_target] Запись Rules выбрала значение, которое нельзя использовать как текстовую цель
    [rules_pattern_match_failed] Не удалось вычислить шаблон PCRE2 Rules
    [rules_zero_width_match] Шаблон Rules создал совпадение нулевой ширины
    [rules_overlapping_capture] Шаблон Rules создал перекрывающиеся текстовые захваты
    [rules_missing_text_capture] Обязательный именованный захват текста не участвовал в совпадении
    [rules_invalid_capture_range] Совпадение или захват Rules находится вне допустимых границ символов UTF-8
    [rules_duplicate_target] Две записи Rules претендуют на одну физическую текстовую цель
    [rules_invalid_materialization] Рецепт проекции Rules не может восстановить исходное значение
    [rules_snapshot_invalid] Извлечённые группы Rules не образуют допустимый снимок ресурсов
    [rules_snapshot_store_failed] Не удалось зафиксировать проверенный снимок извлечения Rules
    [write_back_extraction_out_of_date] Извлечённые ресурсы больше не соответствуют текущему источнику проекта
    [write_back_asset_snapshot_invalid] Сохранённые ресурсы RPG Maker не образуют допустимый снимок обратной записи
    [source_document_invalid] Исходный документ RPG Maker не соответствует требуемому формату
    [generic_source_document_invalid] Исходный документ Generic JSONL не соответствует требуемому формату
    [write_back_mutation_invalid] Проверенное изменение перевода нельзя применить к зафиксированному исходному расположению
    [write_back_output_path_invalid] Перезаписанный файл находится вне разрешённого дерева вывода RPG Maker
    [write_back_output_path_duplicate] Несколько перезаписанных файлов нацелены на один путь вывода
    [write_back_candidate_project_mismatch] Подготовленный кандидат обратной записи принадлежит другому проекту
    [write_back_candidate_invalid] Кандидат обратной записи не соответствует требуемой структуре дерева data/js
    [write_back_not_published] Кандидат обратной записи не заменил текущий каталог вывода
    [write_back_published_with_residuals] Вывод опубликован, но некоторые артефакты восстановления не удалены
    [write_back_recovery_required] Перед использованием содержимого каталога вывода требуется восстановление
    [internal_invariant] Нарушен внутренний инвариант; это дефект ATT
   *[other] __ATT_FALLBACK__
}
diagnostic-io-kind-value = { $code ->
    [not_found] Не найдено
    [permission_denied] Доступ запрещён
    [connection_refused] Соединение отклонено
    [connection_reset] Соединение сброшено
    [host_unreachable] Узел недоступен
    [network_unreachable] Сеть недоступна
    [connection_aborted] Соединение прервано
    [not_connected] Соединение не установлено
    [address_in_use] Адрес уже используется
    [address_not_available] Адрес недоступен
    [network_down] Сеть отключена
    [broken_pipe] Канал разорван
    [already_exists] Уже существует
    [would_block] Операция вызвала бы блокировку
    [not_a_directory] Не является каталогом
    [is_a_directory] Является каталогом
    [directory_not_empty] Каталог не пуст
    [read_only_filesystem] Файловая система доступна только для чтения
    [stale_network_file_handle] Устаревший дескриптор сетевого файла
    [invalid_input] Недопустимые входные данные операции
    [invalid_data] Недопустимые данные
    [timed_out] Время ожидания операции истекло
    [write_zero] Запись не продвинулась
    [storage_full] Хранилище заполнено
    [not_seekable] Объект не поддерживает позиционирование
    [quota_exceeded] Превышена квота хранилища
    [file_too_large] Файл слишком велик для базовой системы
    [resource_busy] Ресурс занят
    [executable_file_busy] Исполняемый файл занят
    [deadlock] Операция вызвала бы взаимную блокировку
    [crosses_devices] Операция пересекает устройства файловой системы
    [too_many_links] Слишком много ссылок файловой системы
    [invalid_filename] Недопустимое имя файла
    [argument_list_too_long] Список аргументов операционной системы слишком длинный
    [interrupted] Операция прервана
    [unsupported] Операция не поддерживается
    [unexpected_eof] Неожиданный конец файла
    [out_of_memory] Операционная система не смогла выделить память
    [other] Другая ошибка операционной системы
   *[unknown] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [unsupported_prompt_locale] Значение должно быть ровно auto в нижнем регистре или поддерживаемой локалью интерфейса BCP 47
    [language_policy_term_blank] Термин языковой политики не должен быть пустым
    [language_policy_term_surrounding_whitespace] Термин языковой политики не должен содержать пробелы по краям
    [language_policy_term_duplicate] Термин языковой политики не должен повторяться
    [quote_repair_candidates_empty] Список вариантов исправления кавычек не должен быть пустым
    [quote_repair_delimiter_invalid] Разделитель исправления кавычек не должен быть буквой, цифрой, пробелом или управляющим символом
    [quote_repair_pair_duplicate] Пара исправления кавычек не должна повторяться
    [quote_repair_delimiter_ambiguous] Разделитель исправления кавычек должен принадлежать ровно одной паре
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
diagnostic-io-reason = Операция { $operation }: { $kind }
diagnostic-io-reason-with-os-code = Операция { $operation }: { $kind } (ОС { $os_code })
diagnostic-io-reason-with-system-message = Операция { $operation }: { $kind }: { $system_message }
diagnostic-io-reason-with-os-code-and-system-message = Операция { $operation }: { $kind } (ОС { $os_code }): { $system_message }
diagnostic-failure-with-detail = { $failure }: { $detail }
diagnostic-invalid-utf8 = Недопустимый UTF-8 в байте { $valid_up_to }, недопустимая длина { $error_len } байт
diagnostic-incomplete-utf8 = Неполная последовательность UTF-8 после байта { $valid_up_to }
diagnostic-toml-failure-value = { $code ->
    [syntax] Недопустимый синтаксис TOML
    [missing_field] Отсутствует обязательное поле конфигурации
    [unknown_field] Конфигурация содержит неизвестное поле
    [duplicate_field] Поле конфигурации объявлено более одного раза
    [type_mismatch] Ожидалось: { $expected }
    [invalid_value] Значение конфигурации нарушает контракт поля
   *[other] __ATT_FALLBACK__
}
diagnostic-toml-expected-kind-value = { $code ->
    [string] строка
    [integer] целое число
    [boolean] логическое значение
    [string_or_boolean] строка или логическое значение
    [string_array] массив строк
    [integer_array] массив целых чисел
    [string_pair_array] массив пар строк
    [table] таблица
    [table_array] массив таблиц
   *[other] __ATT_FALLBACK__
}
diagnostic-invalid-toml = Недопустимый TOML ({ $resource }): { $failure }
diagnostic-invalid-toml-at = Недопустимый TOML в строке { $line }, столбце { $column } ({ $resource }): { $failure }
diagnostic-http-no-details = Запрос к службе модели завершился ошибкой без общедоступных сведений о состоянии HTTP
diagnostic-http-status = Состояние HTTP { $status }
diagnostic-http-retry-after = Retry-After: { $seconds } секунд
diagnostic-http-provider-code = Код ошибки поставщика { $code }
diagnostic-http-provider-type = Тип ошибки поставщика { $kind }
diagnostic-http-provider-message = Сообщение об ошибке поставщика { $message }
diagnostic-http-fact-separator = ;{ " " }
diagnostic-sqlite = Основной код ошибки SQLite { $primary_code }, расширенный код { $extended_code }
diagnostic-windows-status = Операция Windows { $operation } завершилась ошибкой NTSTATUS { $status }
diagnostic-resource = { $resource }: фактическое значение { $actual }
diagnostic-resource-with-maximum = { $resource }: фактическое значение { $actual }, максимум { $maximum }
task-record-title = Задача перевода { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] Завершена
    [partial] Частично завершена
    [unavailable] Недоступна
    [execution_failed] Ошибка выполнения
    [commit_preparation_failed] Ошибка подготовки фиксации
    [commit_not_applied] Фиксация не применена
    [commit_outcome_unknown] Результат фиксации неизвестен
    [not_committed_after_earlier_failure] Не зафиксирована после предыдущей ошибки
    [invalid_result] Недопустимая последовательность результатов Executor
    [cancelled] Отменена
   *[other] { $state }
}
task-record-summary-with-written = `Задача { $ordinal }/{ $total }` · `Попыток: { $attempts }` · `Принято { $accepted }/{ $expected }` · `Записано в { $written } позиций`
task-record-summary-without-written = `Задача { $ordinal }/{ $total }` · `Попыток: { $attempts }` · `Принято { $accepted }/{ $expected }`
task-record-run-id-label = ID запуска:
task-record-started-at-label = Начало:
task-record-duration-label = Общая длительность:
task-record-endpoint-label = Endpoint:
task-record-model-label = Модель:
task-record-custom-parameters-heading = Пользовательские параметры
task-record-attempts-heading = Попытки запроса
task-record-final-result-heading = Итоговый результат
task-record-no-request = Не сформирован запрос к модели, готовый к отправке.
task-record-empty-assistant = Модель вернула пустой объект.
task-record-parse-error = Ошибка разбора: { $kind ->
    [json] недопустимый JSON ответа модели (категория `{ $category }`), строка { $line }, столбец { $column }
    [thinking_not_allowed] этот режим ответа не принимает рассуждение, строка { $line }, столбец { $column }
    [thinking_envelope_missing] отсутствует обязательная оболочка рассуждения, строка { $line }, столбец { $column }
    [thinking_envelope_unclosed] оболочка рассуждения не закрыта, строка { $line }, столбец { $column }
    [thinking_empty] содержимое рассуждения пусто, строка { $line }, столбец { $column }
    [thinking_nested] обнаружена вложенная оболочка рассуждения, строка { $line }, столбец { $column }
    [thinking_repeated] обнаружена повторная оболочка рассуждения, строка { $line }, столбец { $column }
    [markdown_fence_no_body] блок Markdown не содержит тела, строка { $line }, столбец { $column }
    [markdown_fence_unsupported] допускается только один блок Markdown без метки языка или с меткой json, строка { $line }, столбец { $column }
    [markdown_fence_unclosed] блок Markdown не закрыт, строка { $line }, столбец { $column }
   *[markdown_fence_invalid_closing] блок Markdown должен закрываться последней отдельной строкой, строка { $line }, столбец { $column }
}
task-record-attempt-succeeded = Попытка { $number }: успешно; finish reason { $finish_reason }
task-record-attempt-token-usage = ; токены `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ; длительность `{ $duration }`
task-record-attempt-request-id = ; request ID { $request_id }
task-record-attempt-response-id = ; response ID { $response_id }
task-record-attempt-retryable = Попытка { $number }: повторяемая ошибка запроса; диагностика `{ $code }`; длительность `{ $duration }`
task-record-attempt-retry-after = ; Retry-After `{ $duration }`
task-record-attempt-wait-retry = ; повтор через `{ $duration }`
task-record-attempt-wait-completed = ; ожидание `{ $duration }` завершено; следующая попытка не началась
task-record-attempt-wait-cancelled = ; запланировано ожидание `{ $duration }`; отменено во время ожидания
task-record-attempt-failed = Попытка { $number }: ошибка обработки запроса или ответа; диагностика `{ $code }`; длительность `{ $duration }`
task-record-attempt-cancelled = Попытка { $number }: отменена; длительность `{ $duration }`
task-record-structured-reason = Причина: { $reason }
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
task-record-accepted-written = Принято: { $accepted } элементов, записано в { $written } фактических позиций
task-record-accepted-outcome-unknown = Проверено: { $accepted } элементов; результат фиксации базы данных невозможно подтвердить
task-record-rejected-heading = Не принято:
task-record-rejected-item = { $id }: { $reason }
task-record-protocol-diagnostic = Диагностика протокола: { $diagnostic }
task-record-unavailable-reason = Причина недоступности: { $reason }
task-record-task-diagnostic = Диагностика задачи: `{ $code }`; причина { $reason }
task-record-rejection-reason = { $code ->
    [missing] Отсутствует вывод модели
    [duplicate] Повторяющийся вывод модели
    [invalid_shape] { $detail }
    [invalid_shape_array] Перевод должен быть массивом строк
    [invalid_shape_item] Элемент { $line } массива перевода должен быть строкой
    [line_count_mismatch] Число строк не совпадает (ожидалось { $expected }, получено { $actual })
    [invalid_line_text] Строка { $line } содержит недопустимые управляющие символы
    [blank_line_mismatch] Состояние пустоты строки { $line } не совпадает (ожидалось: { $expected_blank ->
        [blank] пустая
       *[other] непустая
    })
    [blank_translation] Перевод пуст
    [no_natural_language_text] В переводе нет текста на естественном языке
    [contains_byte_order_mark] Перевод содержит BOM
    [placeholder_mismatch] Несовпадение заполнителя: { $detail }
    [unexpected_placeholder] Неожиданный заполнитель: { $detail }
    [placeholder_normalization_ambiguous] Неоднозначная нормализация заполнителя: { $detail }
    [source_residual] Обнаружен остаток исходного языка: { $detail }
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason отличается от stop: { $detail }
    [invalid_response] { $detail }
    [invalid_id] Элемент модели { $index } имеет недопустимый ID
    [unknown_id] Элемент модели { $index } вернул неизвестный ID { $detail }
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] Ответ модели невозможно разобрать
    [all_outputs_rejected] Все результаты модели отклонены при проверке
    [recoverable_request_exhausted] Исчерпан бюджет повторов восстанавливаемых запросов
    [retry_after_exceeds_maximum] Retry-After превышает настроенное максимальное ожидание
   *[other] { $code }
}
task-record-duration-seconds = { $value } с
task-record-duration-milliseconds = { $value } мс
