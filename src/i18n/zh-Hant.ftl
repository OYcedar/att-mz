app-about = 使用可重用專案狀態翻譯遊戲與結構化文字
cli-ui-language-help = Help、診斷、進度、結果與專案日誌使用的語言：ar、zh-Hans、zh-Hant、en、fr、ru、es、ja、ko 或 vi
cli-mz-about = 翻譯 RPG Maker MZ 遊戲
cli-mv-about = 翻譯 RPG Maker MV 遊戲
cli-generic-about = 翻譯約定 JSONL 文字
cli-init-about = 初始化或更新一個命名翻譯專案
cli-extract-about = 從專案目前輸入同步原文
cli-translate-about = 使用明確或已儲存的 Profile 翻譯已擷取原文
cli-write-back-about = 將目前譯文寫入專案輸出
cli-manual-about = 使用可編輯 TOML 管理人工譯文
cli-manual-export-about = 匯出目前需要人工補譯的項目
cli-manual-check-about = 唯讀檢查人工譯文 TOML
cli-manual-apply-about = 套用已填寫且有效的人工譯文
cli-project-lua-about = 對專案資料庫執行 Lua 指令碼
cli-project-name-help = 穩定專案名稱
cli-init-path-help = 輸入根目錄；既有專案可重用上次成功路徑
cli-source-language-help = 原文語言 ID
cli-target-language-help = 譯文目標語言 ID
cli-dialogue-width-help = 對話正文每行允許的最大全形字元數
cli-scrolling-width-help = 捲動文字每行允許的最大全形字元數
cli-help-width-help = 說明框每行允許的最大全形字元數
cli-builtin-help = 使用 ATT 內建的 RPG Maker 文字位置
cli-rules-help = 以此 TOML 定義取代 RPG Maker 擷取規則；空規則清單會停用規則
cli-dialogue-rules-help = 取代與 Builtin 搭配使用的 MV 對話姓名投影
cli-profile-help = 翻譯 Profile ID；省略時重用上次成功 Profile
cli-terms-help = 取代專案術語資源
cli-placeholders-help = 取代專案 Placeholder 資源
cli-project-lua-script-help = 要對專案資料庫執行的 Lua 指令碼
cli-project-lua-arguments-help = 在 -- 後傳給 Lua arg[1..] 的 UTF-8 參數
cli-manual-file-help = 人工譯文 TOML 檔案
cli-usage-heading = 用法：
cli-commands-heading = 命令：
cli-options-heading = 選項：
cli-arguments-heading = 引數：
cli-options-metavar = 選項
cli-command-metavar = 命令
cli-print-help = 顯示說明
cli-print-version = 顯示版本
cli-blank-value = 值不可空白。
cli-invalid-positive-integer = 值必須是正整數。
cli-invalid-ui-language-argument = --ui-language 包含無效語言標籤：{ $value }。
cli-unsupported-ui-language-argument = --ui-language 指定了不支援的語言：{ $value }。
cli-invalid-ui-language-environment = ATT_UI_LANGUAGE 包含無效語言標籤：{ $value }。
cli-unsupported-ui-language-environment = ATT_UI_LANGUAGE 指定了不支援的語言：{ $value }。
cli-ui-language-environment-not-unicode = ATT_UI_LANGUAGE 不是有效 Unicode。
cli-unexpected-argument = 未預期的引數：{ $value }。
cli-missing-required-argument = 缺少必要引數：{ $value }。
cli-invalid-value = { $argument } 的值 { $value } 無效。
cli-error-heading = 錯誤：
cli-try-help = 如需更多資訊，請使用 --help。
cli-missing-value = { $argument } 需要提供值。
cli-missing-subcommand = 必須提供一個命令。
cli-argument-conflict = { $argument } 不能與目前其他引數同時使用。
cli-wrong-number-of-values = { $argument } 的值數量不正確。
cli-invalid-utf8 = 命令列引數不是有效 Unicode。
cli-parse-failure = 無法解析命令列。
error-no-executable-extract-owner = 清除後沒有可執行的 Extract owner，因此未儲存方案。
plan-source-explicit = 明確輸入
plan-source-project-state = 專案狀態
plan-source-product-default = 產品行為
notice-init-reuse-path = 未提供來源路徑，已沿用上次成功路徑：{ $path }。
notice-extract-reuse-owners = 未提供擷取範圍，已沿用上次成功方案：{ $owners }。
notice-translate-reuse-profile = 未提供 Profile，已沿用上次成功 Profile：{ $profile }。
notice-owner-disabled = 已停用 owner { $owner }，並將其移出後續自動方案。
warning-rules-command-non-string-skipped = 警告：Rules 規則 { $rule_number } 跳過了 { $skipped_count } 個非字串 command 參數（來源 { $source_file }，code={ $command_code }，parameter={ $parameter }，類型 { $actual_type }）。
warning-manual-layout-required = 警告：以下文字需要人工檢查換行：{ $locations }（區域={ $region }，全形字元上限={ $max_fullwidth_chars }）。
notice-no-model-request = 所有翻譯單元都是最新狀態，本次不需請求模型。
notice-manual-layout = 有 { $count } 個單元需要人工檢查換行。
notice-log-degraded = 專案日誌無法使用或已降級；命令會繼續，結束狀態不受影響。
notice-task-records-degraded = 翻譯任務記錄無法使用或已降級；命令會繼續，結束狀態不受影響。
progress-init-check-project = 正在檢查專案狀態
progress-init-scan-source = 正在掃描遊戲來源
progress-init-build-candidate = 正在建立專案候選
progress-init-converge-database = 正在收斂專案資料庫
progress-init-publish = 正在發佈初始化專案
progress-save-run-plan = 正在儲存成功執行方案
progress-extract-owner = 擷取 owner：{ $owner }
progress-extract-documents = 正在掃描文件
progress-extract-builtin = Builtin 工作單元
progress-extract-rules = Rules 規則
progress-extract-commit = 正在提交擷取資產
progress-generic-init = 正在初始化 Generic 專案
progress-generic-extract = 正在掃描 Generic JSONL 輸入
progress-translate-planning = 正在規劃翻譯工作
progress-translate-confirmed = 已確認翻譯工作
progress-translate-no-work = 不需要呼叫模型
progress-project-lua = 正在執行專案 Lua 程式
progress-write-back-read-assets = 正在讀取已驗收資產
progress-write-back-planning = 正在規劃文件改寫
progress-write-back-documents = 已改寫文件
progress-write-back-validate-candidate = 正在驗證輸出候選
progress-write-back-publish = 正在發佈輸出；中斷後會等待明確終態
progress-finalizing = 正在完成必要收尾
progress-safe-stopping = 正在安全停止；保留最後確認進度
result-init-completed = 初始化完成：{ $project }
result-init-created = 專案狀態：已建立
result-init-unchanged = 專案狀態：無變更
result-init-updated = 專案狀態：已更新
result-init-stale-owners = 需要重新擷取：{ $owners }
result-extract-completed = 擷取完成：{ $project }
result-translate-completed = 翻譯執行完成：{ $project }（Profile：{ $profile }）
result-translate-summary = 翻譯：工作 { $total }，完整 { $complete }，部分 { $partial }，無法使用 { $unavailable }；寫入 { $written } 處，剩餘 { $remaining } 處
result-translate-convergence = 狀態收斂：保留 { $retained }，失效 { $invalidated }，不適用 { $not_applicable }，重用 { $reused }
result-write-back-completed = 寫回完成：{ $project }
result-project-lua-completed = 專案 Lua 執行完成：{ $project }
result-output-directory = 輸出目錄：{ $path }
result-write-back-summary = 寫回：套用譯文 { $translated } 個單元，保留原文 { $original } 個單元；自動換行 { $auto_wrapped } 段，新增換行 { $breaks } 處；續行全形縮排 { $indents } 處；需人工換行 { $manual } 段
result-generic-extract-unchanged = Generic 輸入未變更：{ $files } 個檔案，{ $groups } 個群組，{ $units } 個單元
result-generic-extract-updated = Generic 輸入已更新：{ $files } 個檔案，{ $groups } 個群組，{ $units } 個單元；保留 { $preserved } 條譯文，清除 { $cleared } 條
result-generic-translate-summary = Generic 翻譯：工作 { $total }，完整 { $complete }，部分 { $partial }，無法使用 { $unavailable }；清除 { $cleared }，重用 { $reused }，接受 { $accepted }，寫入 { $written }，衝突 { $conflicted }，回應問題 { $problems }
result-generic-write-back-summary = Generic 寫回：套用譯文 { $translated } 個單元，保留原文 { $original } 個單元
result-symbol-repair-summary = 符號修復：嘗試 { $attempted } 個單元，實際修復 { $repaired } 個，內部略過 { $skipped } 個，替換 { $replacements } 個符號
result-cancelled = 命令已在安全收尾後取消。
result-plan-saved = 已儲存本次成功執行方案。
log-run-started = 命令 { $command } 已開始。
log-run-succeeded = 命令 { $command } 已成功完成。
log-run-failed = 命令 { $command } 失敗。
log-run-outcome-unknown = 命令 { $command } 已結束，但最終結果未知；請依錯誤中的復原位置處理。
log-run-cancelled = 命令 { $command } 已取消。
log-performance-counters = 效能計數：SQLite 事務控制嘗試 { $sqlite_control_attempted_total } 次；完整候選樹驗證開始 { $candidate_validation_started } 次，完成 { $candidate_validation_completed } 次。
log-lua-print = Lua：{ $message }
log-plan-resolved = 命令 { $command } 的方案來自{ $source }。
log-phase-started = 階段開始：{ $phase }。
log-retry-summary = 共執行 { $count } 次重試。
log-translation-task-started = 翻譯工作 { $index }/{ $total } 已開始。
log-translation-task-finished = 翻譯工作 { $index } 已結束，結果為 { $outcome }。
log-run-recovery-required = 命令 { $command } 結束時需要復原；請依診斷中的復原位置處理。
log-phase-completed = 階段已完成：{ $phase }。
log-phase-stopped = { $outcome ->
    [failed] 階段失敗：{ $phase }。
    [cancelled] 階段已取消：{ $phase }。
   *[other] 階段已停止：{ $phase }。
}
log-cancellation-requested = 已要求取消；已確認 { $confirmed }/{ $total } 項。
log-cancellation-requested-indeterminate = 已要求取消；已確認 { $confirmed } 項，總數未知。
log-run-plan-finalized = { $result ->
    [saved] 執行計畫已儲存。
    [not_saved] 執行計畫未儲存。
    [saved_finalization_failed] 執行計畫已儲存，但收尾失敗。
    [outcome_unknown] 執行計畫的最終狀態未知。
   *[other] 執行計畫收尾停止，結果無法辨識。
}
log-translation-finished = { $result ->
    [not_started] 翻譯未開始。
    [no_work] 翻譯結束，沒有需要處理的內容。
    [complete] 翻譯已完成。
    [incomplete] 翻譯結束，但仍有未完成內容。
    [failed] 翻譯失敗。
    [cancelled] 翻譯已取消。
   *[other] 翻譯已停止，結果無法辨識。
}
log-publication-started = 開始發佈至輸出根目錄 { $path }。
log-publication-finished = { $result ->
    [published] 發佈已完成。
    [not_published] 發佈未修改輸出。
    [recovery_required] 發佈已停止，需要復原。
    [outcome_unknown] 發佈的最終狀態未知。
   *[other] 發佈已停止，結果無法辨識。
}
log-project-log-degraded = 專案日誌發生故障；已記錄 { $failure_kinds } 類故障。
log-task-outcome-value = { $outcome ->
    [complete] 完成
    [partial] 部分完成
    [unavailable] 暫時無法使用
    [failed] 失敗
    [not_committed_after_earlier_failure] 因先前失敗未提交
    [cancelled] 已取消
   *[other] 結果無法辨識
}
diagnostic-location = 位置：{ $subject }
diagnostic-explanation = 原因：{ $reason }
diagnostic-resolution = 處理方式：{ $action }
diagnostic-related = 相關錯誤 { $index }：
diagnostic-resolution-value = { $code ->
    [fix_configuration] 修正指出的設定欄位後重試
    [fix_input] 修正指出的輸入後重試
    [fix_placeholder_rules] 修正指出的 Placeholder 規則後重試
    [adjust_manual_layout] 依指出的位置與顯示寬度人工調整換行與版面
    [check_path_and_permissions] 檢查路徑、檔案系統狀態與權限
    [check_project_state] 檢查並修正專案狀態後重試
    [resolve_contention] 等待衝突作業結束後重試
    [check_model_service] 檢查模型服務回應與帳戶配額
    [preserve_recovery_artifacts] 請勿刪除列出的復原產物；先復原輸出，再重試
    [retry] 重試此作業
    [report_bug] 回報此 ATT 缺陷，並說明當時執行的操作
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] 缺少必填值
    [generic_extract_required] 目前 JSONL 輸入與最近一次 Extract 不一致；請重新執行 att generic extract
    [conflicting_values] 提供的值互相衝突
    [invalid_syntax] 值的語法無效
    [invalid_encoding] 文字編碼無效
    [invalid_value] 值不符合必要契約
    [not_found] 必要物件不存在
    [state_mismatch] 已儲存的專案狀態不符合此作業需求
    [unsupported_windows_code_page] Windows 代碼頁不是 UTF-8
    [transaction_rolled_back] 交易失敗，變更已回復
    [transaction_outcome_unknown] 無法確認交易已提交或回復
    [finalization_failed] 作業結果已產生，但收尾失敗
    [rollback_failed] 主要作業失敗，且回復也失敗
    [external_service_rejected] 外部服務拒絕了請求
    [external_service_unavailable] 外部服務目前不可用
    [executor_closed] 執行服務正在關閉或已經關閉
    [concurrent_shutdown] 另一個呼叫端正在關閉執行器
    [executor_state_poisoned] 執行器生命週期狀態已損壞
    [worker_spawn_failed] 作業系統無法建立工作執行緒
    [worker_channel_closed] 工作執行緒命令通道在收尾完成前關閉
    [worker_panicked] 工作執行緒意外終止
    [reparse_point_forbidden] 路徑包含不可信任的重新解析點
    [non_local_volume] 路徑不在本機固定磁碟區上
    [non_ntfs_volume] 路徑不在 NTFS 磁碟區上
    [case_sensitive_directory] 目錄啟用了區分大小寫的名稱語意
    [lock_cancelled] 等待必要鎖定時作業被取消
    [target_already_exists] 目的地已存在
    [file_identity_changed] 作業期間檔案識別已變更
    [invalid_path] 路徑不是此作業的有效目標
    [wrong_publisher_instance] 發佈權杖屬於另一個發佈器執行個體
    [journal_corrupt] 發佈復原日誌無效或不完整
    [unexpected_artifact] 非預期的檔案系統產物阻擋了作業
    [interactive_session_already_open] 另一個互動式 SQLite 工作階段已在執行
    [backup_incomplete] SQLite 備份未達到完成狀態
    [request_serialization_failed] 無法序列化模型請求
    [response_parsing_failed] 模型回應不是有效的 JSON
    [invalid_response_contract] 模型回應不符合必要的回應契約
    [transport_failed] 收到有效回應前 HTTP 傳輸失敗
    [lua_compilation_failed] 無法編譯 Lua 主程式
    [lua_execution_failed] Lua 主程式執行時失敗
    [rules_pattern_match_failed] 無法評估 Rules 的 PCRE2 模式
    [rules_zero_width_match] Rules 模式產生了零寬度相符項目
    [rules_overlapping_capture] Rules 模式產生了重疊的文字擷取
    [rules_missing_text_capture] 必要的具名文字擷取未參與比對
    [rules_invalid_capture_range] Rules 相符項目或擷取範圍超出有效 UTF-8 字元邊界
    [write_back_candidate_invalid] 寫回候選不符合必要的 data/js 樹狀結構
    [write_back_recovery_required] 必須先復原輸出目錄，才能信任其中內容
    [already_exists] 目標物件已存在
    [cancelled] 操作已取消
    [concurrent_modification] 專案狀態在操作期間遭到並行修改
    [duplicate_identifier] 識別碼重複
    [extraction_out_of_date] 已儲存的提取結果不再符合目前來源
    [invalid_content] 內容不符合必要契約
    [manual_layout_required] 需要手動調整換行或版面
    [operation_failed] 操作失敗
    [placeholder_projection_failed] Placeholder 投影未保留必要結構
    [profile_not_found] 所選翻譯 Profile 不存在
    [recovery_required] 必須先完成復原，才能信任該結果
    [resource_limit] 已達到所需資源限制
    [resource_limit_exceeded] 操作超出後端資源限制
    [source_snapshot_mismatch] 來源不再符合已儲存的快照
    [unavailable] 要求的工作暫時無法使用
    [internal_invariant] 內部不變條件遭破壞；這是 ATT 缺陷
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] 語言原則詞彙不能為空白
    [language_policy_term_surrounding_whitespace] 語言原則詞彙不能包含前後空白
    [language_policy_term_duplicate] 語言原則詞彙不能重複
    [language_id_blank] 語言 ID 不能為空白
    [language_id_surrounding_whitespace] 語言 ID 不能包含前後空白
    [language_id_uses_underscore] 語言 ID 的子標籤間必須使用連字號
    [language_id_invalid_syntax] 語言 ID 必須符合 RFC 5646 語法
    [language_id_invalid_registry_tag] 語言 ID 包含無效的登錄子標籤
    [language_id_canonicalization_failed] 無法正規化語言 ID
    [language_id_undefined_primary_language] 語言 ID 必須定義主要語言
    [language_id_duplicate] 語言 ID 必須唯一
    [language_catalog_empty] 至少需要一個來源語言模組
    [url_invalid] 值必須是有效的 URL
    [url_credentials_forbidden] URL 不能包含認證資訊
    [url_fragment_forbidden] URL 不能包含片段
    [url_scheme_unsupported] URL 配置必須是 http 或 https
    [api_key_blank] API key 不能為空白
    [api_key_surrounding_whitespace] API key 不能包含前後空白
    [api_key_invalid_header] API key 無法表示為 HTTP Header 值
    [strict_json_invalid] 值必須是嚴格 JSON（列={ $line }，欄={ $column }）
    [json_object_required] 值必須是 JSON 物件
    [reserved_request_field] 此欄位由請求協定擁有，不能覆寫
    [proxy_must_be_false_or_url] proxy 必須是 false 或完整的 http/https URL
    [pem_path_duplicate] PEM 路徑必須唯一
    [runtime_maximum_exceeded] 值超過執行階段上限（實際值={ $actual }，上限={ $maximum }）
    [value_surrounding_whitespace] 值不能包含前後空白
    [value_blank] 值不能為空白
    [path_blank] 路徑不能為空
    [positive_required] 值必須大於零（實際值={ $actual }）
    [usize_range_exceeded] 值超過此平台的 usize 範圍（實際值={ $actual }）
    [u32_range_exceeded] 值超過 u32 範圍（實際值={ $actual }）
    [duplicate_profile_id] 翻譯設定檔 ID 必須唯一
    [selected_profile_invalid] 所選翻譯設定檔的結構或欄位類型無效
    [referenced_client_not_found] 參照的 LLM 用戶端不存在
   *[other] __ATT_FALLBACK__
}
diagnostic-http-status = HTTP 狀態 { $status }
diagnostic-retry-after = Retry-After：{ $seconds } 秒
diagnostic-provider-code = 服務方 code：{ $code }
diagnostic-provider-type = 服務方 type：{ $kind }
diagnostic-provider-message = 服務方訊息：{ $message }
diagnostic-json-position = 第 { $line } 行，第 { $column } 欄
diagnostic-placeholder-rule-file = { $path } 中的 Placeholder 規則 { $number }
diagnostic-placeholder-rule-project = 目前專案的 Placeholder 規則 { $number }
manual-exported = 已匯出 { $entries } 筆：{ $path }
manual-checked = 有效 { $valid }，未填寫 { $unfilled }，錯誤 { $errors }
manual-applied = 已套用 { $applied }，未填寫 { $unfilled }，錯誤 { $errors }
manual-issue = { $object }：{ $reason }；{ $help }。
manual-value = { $code ->
    [invalid_source_line] source 第 { $line } 項包含換行或 NUL
    [invalid_translation_line] translation 第 { $line } 項包含換行或 NUL
    [fixed_length] fixed 譯文需要 { $expected } 項，目前為 { $actual } 項
    [fixed_blank_slot] fixed 譯文第 { $line } 項必須保留空槽
    [rerun_export] 重新執行 manual export
    [rerun_export_without_controls] 重新執行 manual export，不要把換行或 NUL 寫進陣列項目
    [rerun_export_then_fill] 重新執行 manual export 後再填寫譯文
    [keep_exported_type] 保留 manual export 產生的 type
   *[other] __ATT_FALLBACK__
}
task-record-title = 翻譯任務 { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] 完成
    [partial] 部分完成
    [unavailable] 不可用
    [execution_failed] 執行失敗
    [commit_preparation_failed] 提交準備失敗
    [commit_not_applied] 提交未套用
    [commit_outcome_unknown] 提交結果未知
    [not_committed_after_earlier_failure] 因先前失敗未提交
    [invalid_result] 執行結果序列無效
    [cancelled] 已取消
   *[other] { $state }
}
task-record-summary-with-written = `任務 { $ordinal }/{ $total }` · `嘗試 { $attempts } 次` · `驗收 { $accepted }/{ $expected }` · `寫入 { $written } 處`
task-record-summary-without-written = `任務 { $ordinal }/{ $total }` · `嘗試 { $attempts } 次` · `驗收 { $accepted }/{ $expected }`
task-record-run-id-label = Run ID：
task-record-started-at-label = 開始時間：
task-record-duration-label = 總耗時：
task-record-endpoint-label = Endpoint：
task-record-model-label = Model：
task-record-custom-parameters-heading = 自訂參數
task-record-attempts-heading = 請求過程
task-record-final-result-heading = 最終結果
task-record-no-request = 沒有形成可傳送的模型請求。
task-record-parse-error = 解析錯誤：{ $kind ->
    [thinking_empty] 模型回應的思考內容為空，第 { $line } 行、第 { $column } 欄
   *[json] 模型回應 JSON 無效（類別 `{ $category }`），第 { $line } 行、第 { $column } 欄
}
task-record-attempt-succeeded = 嘗試 { $number }：成功；finish reason { $finish_reason }
task-record-attempt-token-usage = ；token `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ；耗時 `{ $duration }`
task-record-attempt-retryable = 嘗試 { $number }：可重試請求失敗；耗時 `{ $duration }`
task-record-attempt-retry-after = ；Retry-After `{ $duration }`
task-record-attempt-wait-retry = ；等待 `{ $duration }` 後重試
task-record-attempt-wait-completed = ；等待 `{ $duration }` 已完成，下一次嘗試未開始
task-record-attempt-wait-cancelled = ；計畫等待 `{ $duration }`，等待期間取消
task-record-attempt-failed = 嘗試 { $number }：請求或回應處理失敗；耗時 `{ $duration }`
task-record-attempt-cancelled = 嘗試 { $number }：已取消；耗時 `{ $duration }`
task-record-final-status = 狀態：{ $state ->
    [complete] 完成，已確認提交
    [partial] 部分完成，已確認提交
    [unavailable] 不可用，專案未變更
    [execution_failed] 執行失敗，未提交
    [commit_preparation_failed] 提交準備失敗，確定未套用
    [commit_not_applied] 交易確定未套用
    [commit_outcome_unknown] 提交結果未知
    [not_committed_after_earlier_failure] 因先前任務失敗而未提交
    [invalid_result] Executor 結果序列無效，未提交
    [cancelled] 已取消，未提交
   *[other] { $state }
}
task-record-accepted-written = 已接受：{ $accepted } 項，寫入 { $written } 個實際位置
task-record-accepted-outcome-unknown = 已驗收：{ $accepted } 項；無法確認資料庫提交終態
task-record-task-diagnostic = 任務診斷
task-record-duration-seconds = { $value } 秒
task-record-duration-milliseconds = { $value } 毫秒
