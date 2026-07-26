app-about = Перевод игр RPG Maker с повторно используемым состоянием проекта
cli-config-help = Строгий файл конфигурации TOML для этого запуска
cli-ui-language-help = Язык справки, диагностики, прогресса, результатов и журналов проекта: ar, zh-Hans, zh-Hant, en, fr, ru, es, ja, ko или vi
cli-progress-help = Режим текущего прогресса: auto, plain или off
cli-mz-about = Перевести игру RPG Maker MZ
cli-mv-about = Перевести игру RPG Maker MV
cli-init-about = Инициализировать или обновить именованный игровой проект
cli-extract-about = Извлечь текст по явному или сохранённому плану owner
cli-translate-about = Перевести извлечённый текст с явным или сохранённым Profile
cli-write-back-about = Записать принятые переводы обратно в игру
cli-project-lua-about = Однократно выполнить доверенную программу Lua в контексте проекта
cli-project-name-help = Стабильное имя проекта
cli-init-path-help = Корень игры RPG Maker; существующий проект может повторно использовать последний успешный путь
cli-source-language-help = ID исходного языка
cli-target-language-help = ID целевого языка
cli-dialogue-width-help = Максимум полноширинных символов в строке диалога
cli-scrolling-width-help = Максимум полноширинных символов в строке прокручиваемого текста
cli-help-width-help = Максимум полноширинных символов в строке справки или описания
cli-builtin-help = Использовать встроенные в ATT позиции текста RPG Maker
cli-rules-help = Заменить owner Rules этим TOML; пустой список правил отключает его
cli-dialogue-rules-help = Заменить проекцию имён диалога MV, используемую с Builtin
cli-lua-help = Заменить программу Lua этапа; файл нулевого размера очищает её
cli-profile-help = ID Profile перевода; при отсутствии используется последний успешный Profile
cli-terms-help = Заменить терминологический ресурс проекта
cli-placeholders-help = Заменить ресурс Placeholder проекта
cli-project-lua-profile-help = Profile для ручной приёмки Standard; если он не указан, при открытии Standard используется последний успешный Translate Profile
cli-project-lua-script-help = Доверенная программа Lua для однократного выполнения
cli-project-lua-arguments-help = Аргумент UTF-8 для Lua arg[1..] после --
cli-usage-heading = Использование:
cli-commands-heading = Команды:
cli-options-heading = Параметры:
cli-arguments-heading = Аргументы:
cli-options-metavar = ПАРАМЕТРЫ
cli-command-metavar = КОМАНДА
cli-print-help = Показать справку
cli-print-version = Показать версию
cli-missing-config = Отсутствует обязательный путь конфигурации --config <FILE>.
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
log-label-phase-plan-standard = планирование стандартной записи
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
notice-translate-reuse-lua = Параметр Lua не указан; используется последний успешный выбор Translate Lua.
notice-write-back-reuse-lua = Параметр Lua не указан; используется последняя успешная программа WriteBack Lua.
notice-write-back-standard-only = Программа WriteBack Lua не настроена; выполняется только Standard.
notice-owner-disabled = Owner { $owner } отключён и удалён из будущих автоматических планов.
notice-lua-cleared = Программа Lua { $phase } очищена и в этот раз выполняться не будет.
notice-no-model-request = Все единицы стандартного перевода актуальны; в этом запуске Standard не отправлял запрос к модели.
notice-manual-layout = { $count ->
    [one] 1 единица требует ручной проверки переноса строк.
    [few] { $count } единицы требуют ручной проверки переноса строк.
    [many] { $count } единиц требуют ручной проверки переноса строк.
   *[other] { $count } единицы требуют ручной проверки переноса строк.
}
notice-log-degraded = Журнал проекта недоступен или работает с ошибками; команда продолжится, а код выхода не изменится.
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
progress-extract-lua = Выполнение программы Extract Lua
progress-extract-commit = Commit извлечённых ресурсов
progress-translate-planning = Планирование задач перевода
progress-translate-confirmed = Подтверждено задач перевода
progress-translate-no-work = Запрос к модели не нужен
progress-project-lua = Выполнение программы Lua проекта
progress-write-back-read-assets = Чтение принятых ресурсов
progress-write-back-planning = Планирование перезаписи документов
progress-write-back-documents = Перезаписано документов
progress-write-back-lua = Выполнение программы WriteBack Lua
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
result-translate-standard = Стандартный перевод: { $total } задач; завершено { $complete }, частично { $partial }, недоступно { $unavailable }; записано { $written } позиций, осталось { $remaining }
result-translate-convergence = Сведение состояния: сохранено { $retained }, аннулировано { $invalidated }, неприменимо { $not_applicable }, переиспользовано { $reused }
result-write-back-completed = Запись завершена: { $project }
result-project-lua-completed = Выполнение Lua проекта завершено: { $project }
result-output-directory = Каталог вывода: { $path }
result-write-back-standard = Стандартная запись: { $translated } переведённых единиц, { $original } исходных; автоперенос { $auto_wrapped }, добавлено переносов { $breaks } и полноширинных отступов { $indents }; ручная раскладка { $manual }
result-lua-executed = Lua: выполнено
result-lua-not-executed = Lua: не выполнено
result-cancelled = Команда отменена после безопасного завершения.
result-plan-saved = Успешный план запуска сохранён.
result-translate-plan-sources = План этого успешного запуска сохранён. Источник Profile: { $profile_source }; источник Lua: { $lua_source }.
log-run-started = Команда { $command } запущена.
log-run-succeeded = Команда { $command } успешно завершена.
log-run-failed = Команда { $command } завершилась ошибкой.
log-run-outcome-unknown = Команда { $command } завершилась, но итоговое состояние неизвестно; используйте пути восстановления из ошибки.
log-run-cancelled = Команда { $command } отменена.
log-performance-counters = Счётчики производительности: попыток управления транзакциями SQLite — { $sqlite_control_attempted_total }; полных проверок дерева-кандидата начато — { $candidate_validation_started }, завершено — { $candidate_validation_completed }.
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
    [process_output] Вывод процесса
    [lua] Выполнение Lua проекта
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
    [tag_value_contains_closing_delimiter] Строка { $line } содержит '>', преждевременно закрывающий значение тега
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
