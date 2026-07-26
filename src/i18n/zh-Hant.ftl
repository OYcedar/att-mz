app-about = 使用可重用專案狀態翻譯 RPG Maker 遊戲
cli-config-help = 本次執行使用的嚴格 TOML 設定檔
cli-ui-language-help = Help、診斷、進度、結果與專案日誌使用的語言：ar、zh-Hans、zh-Hant、en、fr、ru、es、ja、ko 或 vi
cli-progress-help = 即時進度模式：auto、plain 或 off
cli-mz-about = 翻譯 RPG Maker MZ 遊戲
cli-mv-about = 翻譯 RPG Maker MV 遊戲
cli-init-about = 初始化或更新一個命名遊戲專案
cli-extract-about = 使用明確或已儲存的 owner 方案擷取原文
cli-translate-about = 使用明確或已儲存的 Profile 翻譯已擷取原文
cli-write-back-about = 將已驗收譯文寫回遊戲
cli-project-lua-about = 在專案上下文中一次性執行可信 Lua 程式
cli-project-name-help = 穩定專案名稱
cli-init-path-help = RPG Maker 遊戲根目錄；既有專案可重用上次成功路徑
cli-source-language-help = 原文語言 ID
cli-target-language-help = 譯文目標語言 ID
cli-dialogue-width-help = 對話正文每行允許的最大全形字元數
cli-scrolling-width-help = 捲動文字每行允許的最大全形字元數
cli-help-width-help = 說明框每行允許的最大全形字元數
cli-builtin-help = 使用 ATT 內建的 RPG Maker 文字位置
cli-rules-help = 以此 TOML 定義取代 Rules owner；空規則清單會停用它
cli-dialogue-rules-help = 取代與 Builtin 搭配使用的 MV 對話姓名投影
cli-lua-help = 取代目前階段的 Lua 程式；零位元組檔案會清除它
cli-profile-help = 翻譯 Profile ID；省略時重用上次成功 Profile
cli-terms-help = 取代專案術語資源
cli-placeholders-help = 取代專案 Placeholder 資源
cli-project-lua-profile-help = Standard 人工驗收使用的 Profile；省略時在開啟 Standard 能力時重用上次成功的 Translate Profile
cli-project-lua-script-help = 本次一次性執行的可信 Lua 程式
cli-project-lua-arguments-help = 在 -- 後傳給 Lua arg[1..] 的 UTF-8 參數
cli-usage-heading = 用法：
cli-commands-heading = 命令：
cli-options-heading = 選項：
cli-arguments-heading = 引數：
cli-options-metavar = 選項
cli-command-metavar = 命令
cli-print-help = 顯示說明
cli-print-version = 顯示版本
cli-missing-config = 缺少必要的設定路徑 --config <FILE>。
cli-blank-value = 值不可空白。
cli-invalid-positive-integer = 值必須是正整數。
cli-invalid-progress = 不支援進度模式 { $value }；請使用 auto、plain 或 off。
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
log-label-phase-check-project = 檢查專案
log-label-phase-scan-source = 掃描來源
log-label-phase-prepare-candidate = 準備候選目錄
log-label-phase-update-database = 更新資料庫
log-label-phase-publish = 發布結果
log-label-phase-builtin = 內建擷取
log-label-phase-rules = 規則擷取
log-label-phase-lua = Lua 處理
log-label-phase-planning = 規劃工作
log-label-phase-confirmed-tasks = 確認工作
log-label-phase-no-work = 無需處理
log-label-phase-read-assets = 讀取資產
log-label-phase-plan-standard = 規劃標準寫回
log-label-phase-rewrite-documents = 改寫文件
log-label-phase-validate-candidate = 驗證候選目錄
log-label-task-complete = 完整
log-label-task-partial = 部分可用
log-label-task-unavailable = 無法使用
log-label-task-failed = 失敗
error-state-applied-finalization = 結果已生效，但收尾失敗。重試前請先檢查專案狀態。
error-no-executable-extract-owner = 清除後沒有可執行的 Extract owner，因此未儲存方案。
error-plan-save-failed-applied = 命令結果已生效，但新執行方案未儲存。下次請明確傳入預期選項。
error-plan-save-outcome-unknown = 命令結果已生效，但無法確認執行方案提交結果。下次請明確傳入預期選項。
plan-source-explicit = 明確輸入
plan-source-project-state = 專案狀態
plan-source-product-default = 產品行為
notice-init-reuse-path = 未提供來源路徑，已沿用上次成功路徑：{ $path }。
notice-extract-reuse-owners = 未提供擷取範圍，已沿用上次成功方案：{ $owners }。
notice-translate-reuse-profile = 未提供 Profile，已沿用上次成功 Profile：{ $profile }。
notice-translate-reuse-lua = 未提供 Lua 選項，已沿用上次成功的 Translate Lua 選擇。
notice-write-back-reuse-lua = 未提供 Lua 選項，已沿用上次成功的 WriteBack Lua 程式。
notice-write-back-standard-only = 尚未設定 WriteBack Lua 程式，本次僅執行 Standard。
notice-owner-disabled = 已停用 owner { $owner }，並將其移出後續自動方案。
notice-lua-cleared = 已清除 { $phase } Lua 程式，本輪不會執行。
notice-no-model-request = 所有標準翻譯單元都是最新狀態，Standard 本次未請求模型。
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
progress-extract-lua = 正在執行 Extract Lua 程式
progress-extract-commit = 正在提交擷取資產
progress-translate-planning = 正在規劃翻譯工作
progress-translate-confirmed = 已確認翻譯工作
progress-translate-no-work = 不需要呼叫模型
progress-project-lua = 正在執行專案 Lua 程式
progress-write-back-read-assets = 正在讀取已驗收資產
progress-write-back-planning = 正在規劃文件改寫
progress-write-back-documents = 已改寫文件
progress-write-back-lua = 正在執行 WriteBack Lua 程式
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
result-translate-standard = 標準翻譯：工作 { $total }，完整 { $complete }，部分 { $partial }，無法使用 { $unavailable }；寫入 { $written } 處，剩餘 { $remaining } 處
result-translate-convergence = 狀態收斂：保留 { $retained }，失效 { $invalidated }，不適用 { $not_applicable }，重用 { $reused }
result-write-back-completed = 寫回完成：{ $project }
result-project-lua-completed = 專案 Lua 執行完成：{ $project }
result-output-directory = 輸出目錄：{ $path }
result-write-back-standard = 標準寫回：套用譯文 { $translated } 個單元，保留原文 { $original } 個單元；自動換行 { $auto_wrapped } 段，新增換行 { $breaks } 處；續行全形縮排 { $indents } 處；需人工換行 { $manual } 段
result-lua-executed = Lua：已執行
result-lua-not-executed = Lua：未執行
result-cancelled = 命令已在安全收尾後取消。
result-plan-saved = 已儲存本次成功執行方案。
result-translate-plan-sources = 已儲存本次成功執行方案。Profile 來源：{ $profile_source }；Lua 來源：{ $lua_source }。
log-run-started = 命令 { $command } 已開始。
log-run-succeeded = 命令 { $command } 已成功完成。
log-run-failed = 命令 { $command } 失敗。
log-run-outcome-unknown = 命令 { $command } 已結束，但最終結果未知；請依錯誤中的復原位置處理。
log-run-cancelled = 命令 { $command } 已取消。
log-performance-counters = 效能計數：SQLite 事務控制嘗試 { $sqlite_control_attempted_total } 次；完整候選樹驗證開始 { $candidate_validation_started } 次，完成 { $candidate_validation_completed } 次。
log-plan-resolved = 命令 { $command } 的方案來自{ $source }。
log-phase-started = 階段開始：{ $phase }。
log-phase-finished = 階段完成：{ $phase }。
log-retry-summary = 共執行 { $count } 次重試。
log-no-work = 不需執行工作：{ $reason }。
log-no-work-translation-up-to-date = 譯文已與目前來源和設定檔一致
log-partial-result = 有 { $count } 個部分結果需要關注。
log-translation-task-started = 翻譯工作 { $index }/{ $total } 已開始。
log-translation-task-finished = 翻譯工作 { $index } 已結束，結果為 { $outcome }。
log-translation-task-diagnostic = 翻譯工作 { $index } 在嘗試 { $attempts } 次後回報診斷：{ $diagnostic }
diagnostic-title = 錯誤 [{ $code }]
diagnostic-stage = 階段：{ $stage }
diagnostic-subject = 位置：{ $subject }
diagnostic-subject-value = { $kind ->
    [command] 指令 { $value }
    [field] 欄位 { $value }
    [project] 專案 { $value }
    [profile] 設定檔 { $value }
    [component] 元件 { $value }
   *[other] { $value }
}
diagnostic-reason = 原因：{ $reason }
diagnostic-impact = 影響：{ $impact }
diagnostic-action = 處理方式：{ $action }
diagnostic-recovery = 復原位置：{ $recovery }
diagnostic-recovery-value = { $kind ->
    [component] 元件 { $value }
    [transaction] 交易 { $value }
   *[other] { $value }
}
diagnostic-related = 相關錯誤 { $index }：
diagnostic-stage-value = { $code ->
    [process_startup] 處理程序啟動
    [process_output] 處理程序輸出
    [configuration] 設定載入
    [command_preparation] 指令準備
    [project_opening] 開啟專案
    [init] 初始化
    [extract] 擷取
    [translate] 翻譯
    [write_back] 寫回
    [lua] 專案 Lua 執行
    [model_request] 模型請求
    [run_plan_finalization] 執行計畫收尾
    [publication] 發佈
    [shutdown] 關閉
    [logging] 專案記錄
   *[other] __ATT_FALLBACK__
}
diagnostic-impact-value = { $code ->
    [unchanged] 狀態未變更
    [valid_progress_preserved] 已保留有效進度
    [result_applied_but_run_plan_not_saved] 結果已套用，但執行計畫未儲存
    [state_applied_but_finalization_failed] 狀態已套用，但收尾未完成
    [recovery_required] 必須先復原，才能信任目前狀態
    [outcome_unknown] 最終狀態未知
   *[other] __ATT_FALLBACK__
}
diagnostic-action-value = { $code ->
    [fix_configuration] 修正指出的設定欄位後重試
    [fix_input] 修正指出的輸入後重試
    [check_path_and_permissions] 檢查路徑、檔案系統狀態與權限
    [check_project_state] 檢查並修正專案狀態後重試
    [retry_after_resolving_contention] 等待衝突作業結束後重試
    [check_model_service] 檢查模型服務回應與帳戶配額
    [preserve_recovery_artifacts] 請勿刪除列出的復原產物；先復原輸出，再重試
    [retry] 重試此作業
    [report_bug] 附上錯誤碼與記錄路徑，回報此 ATT 缺陷
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] 缺少必填值
    [extract_plan_required] 專案沒有可重用的 Extract 計畫；請提供 --builtin、--rules 或 --lua 中的至少一項
    [conflicting_values] 提供的值互相衝突
    [invalid_syntax] 值的語法無效
    [invalid_encoding] 文字編碼無效
    [invalid_value] 值不符合必要契約
    [not_found] 必要物件不存在
    [busy] 資源正由其他作業持有
    [state_mismatch] 已儲存的專案狀態不符合此作業需求
    [requirement_failed] 必要前置條件未滿足
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
    [lua_database_open_failed] Lua 主機無法開啟專案資料庫工作階段
    [lua_context_creation_failed] Lua 執行階段無法建立 VM 內容
    [lua_compilation_failed] 無法編譯 Lua 主程式
    [lua_execution_failed] Lua 主程式執行時失敗
    [lua_host_call_failed] Lua 主機功能呼叫失敗
    [lua_finalization_failed] Lua 主機無法完成所有已繫結資源的收尾
    [lua_unclosed_transaction] Lua 程式結束時交易仍開啟；該交易已回復
    [lua_snapshot_store_failed] 無法提交已驗證的 Lua 擷取快照
    [rules_definition_invalid] Rules 程式不符合 Rules 定義契約
    [rules_document_read_failed] 無法讀取 Rules 程式需要的來源文件
    [rules_no_non_blank_match] Rules 項目未產生任何非空白語意單元
    [rules_invalid_target] Rules 項目選取了不能作為文字目標的值
    [rules_pattern_match_failed] 無法評估 Rules 的 PCRE2 模式
    [rules_zero_width_match] Rules 模式產生了零寬度相符項目
    [rules_overlapping_capture] Rules 模式產生了重疊的文字擷取
    [rules_missing_text_capture] 必要的具名文字擷取未參與比對
    [rules_invalid_capture_range] Rules 相符項目或擷取範圍超出有效 UTF-8 字元邊界
    [rules_duplicate_target] 兩個 Rules 項目宣告了相同的實體文字目標
    [rules_invalid_materialization] Rules 投影配方無法重建來源值
    [rules_snapshot_invalid] 擷取出的 Rules 群組無法組成有效的資產快照
    [rules_snapshot_store_failed] 無法提交已驗證的 Rules 擷取快照
    [write_back_extraction_out_of_date] 已擷取資產不再符合目前專案來源
    [write_back_asset_snapshot_invalid] 已儲存的 Standard 資產無法組成有效的寫回快照
    [source_document_invalid] RPG Maker 來源文件不符合必要的文件格式
    [write_back_mutation_invalid] 已驗證的翻譯變更無法套用到凍結的來源位置
    [write_back_output_path_invalid] 重寫檔案位於允許的 RPG Maker 輸出樹之外
    [write_back_output_path_duplicate] 多個重寫檔案指向相同輸出路徑
    [write_back_candidate_project_mismatch] 已準備的寫回候選屬於另一個專案
    [write_back_candidate_invalid] 寫回候選不符合必要的 data/js 樹狀結構
    [write_back_unexpected_lua_outcome] Lua 寫回程式傳回了其他 Lua 階段的結果
    [write_back_not_published] 寫回候選未取代目前的輸出目錄
    [write_back_published_with_residuals] 輸出已發佈，但部分復原產物無法移除
    [write_back_recovery_required] 必須先復原輸出目錄，才能信任其中內容
    [internal_invariant] 內部不變條件遭破壞；這是 ATT 缺陷
   *[other] __ATT_FALLBACK__
}
diagnostic-io-kind-value = { $code ->
    [not_found] 找不到物件
    [permission_denied] 權限不足
    [connection_refused] 連線遭拒
    [connection_reset] 連線已重設
    [host_unreachable] 無法連線到主機
    [network_unreachable] 無法連線到網路
    [connection_aborted] 連線已中止
    [not_connected] 尚未連線
    [address_in_use] 位址已在使用中
    [address_not_available] 位址不可用
    [network_down] 網路已中斷
    [broken_pipe] 管道已中斷
    [already_exists] 物件已存在
    [would_block] 作業會封鎖
    [not_a_directory] 物件不是目錄
    [is_a_directory] 物件是目錄
    [directory_not_empty] 目錄不是空的
    [read_only_filesystem] 檔案系統是唯讀的
    [stale_network_file_handle] 網路檔案控制代碼已失效
    [invalid_input] 作業輸入無效
    [invalid_data] 資料無效
    [timed_out] 作業逾時
    [write_zero] 寫入沒有進展
    [storage_full] 儲存空間已滿
    [not_seekable] 物件不支援搜尋位置
    [quota_exceeded] 儲存配額已用盡
    [file_too_large] 檔案超過底層系統可處理的大小
    [resource_busy] 資源忙碌中
    [executable_file_busy] 可執行檔正在使用中
    [deadlock] 作業會造成死結
    [crosses_devices] 作業跨越檔案系統裝置
    [too_many_links] 檔案系統連結過多
    [invalid_filename] 檔名無效
    [argument_list_too_long] 作業系統引數清單太長
    [interrupted] 作業已中斷
    [unsupported] 不支援此作業
    [unexpected_eof] 檔案意外結束
    [out_of_memory] 作業系統無法配置記憶體
    [other] 其他作業系統錯誤
   *[unknown] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [runtime_configuration_invalid] 執行階段設定無效
    [unsupported_prompt_locale] 必須是全小寫 auto 或支援的 BCP 47 介面語言
    [language_policy_term_blank] 語言原則詞彙不能為空白
    [language_policy_term_surrounding_whitespace] 語言原則詞彙不能包含前後空白
    [language_policy_term_duplicate] 語言原則詞彙不能重複
    [quote_repair_candidates_empty] 引號修復候選清單不能為空
    [quote_repair_delimiter_invalid] 引號修復分隔符號不能是英數字元、空白或控制字元
    [quote_repair_pair_duplicate] 引號修復配對不能重複
    [quote_repair_delimiter_ambiguous] 引號修復分隔符號必須只屬於一個配對
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
diagnostic-io-reason = 作業 { $operation }：{ $kind }
diagnostic-io-reason-with-os-code = 作業 { $operation }：{ $kind }（OS { $os_code }）
diagnostic-io-reason-with-system-message = 作業 { $operation }：{ $kind }：{ $system_message }
diagnostic-io-reason-with-os-code-and-system-message = 作業 { $operation }：{ $kind }（OS { $os_code }）：{ $system_message }
diagnostic-failure-with-detail = { $failure }：{ $detail }
diagnostic-invalid-utf8 = 第 { $valid_up_to } 位元組的 UTF-8 無效，無效長度為 { $error_len } 位元組
diagnostic-incomplete-utf8 = 第 { $valid_up_to } 位元組後是未完成的 UTF-8 序列
diagnostic-toml-failure-value = { $code ->
    [syntax] TOML 語法無效
    [missing_field] 缺少必填設定欄位
    [unknown_field] 設定包含未知欄位
    [duplicate_field] 設定欄位被重複宣告
    [type_mismatch] 應為{ $expected }
    [invalid_value] 設定值不符合欄位契約
   *[other] __ATT_FALLBACK__
}
diagnostic-toml-expected-kind-value = { $code ->
    [string] 字串
    [integer] 整數
    [boolean] 布林值
    [string_or_boolean] 字串或布林值
    [string_array] 字串陣列
    [integer_array] 整數陣列
    [string_pair_array] 字串配對陣列
    [table] 表格
    [table_array] 表格陣列
   *[other] __ATT_FALLBACK__
}
diagnostic-invalid-toml = TOML 無效（{ $resource }）：{ $failure }
diagnostic-invalid-toml-at = TOML 第 { $line } 列、第 { $column } 欄無效（{ $resource }）：{ $failure }
diagnostic-http-no-details = 模型服務請求失敗，但未傳回可公開的 HTTP 狀態詳細資料
diagnostic-http-status = HTTP 狀態碼 { $status }
diagnostic-http-retry-after = Retry-After { $seconds } 秒
diagnostic-http-provider-code = 供應商錯誤碼 { $code }
diagnostic-http-provider-type = 供應商錯誤類型 { $kind }
diagnostic-http-fact-separator = ；
diagnostic-sqlite = SQLite 主要錯誤碼 { $primary_code }，延伸錯誤碼 { $extended_code }
diagnostic-windows-status = Windows 作業 { $operation } 失敗，NTSTATUS { $status }
diagnostic-resource = { $resource }：實際值 { $actual }
diagnostic-resource-with-maximum = { $resource }：實際值 { $actual }，上限 { $maximum }
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
task-record-empty-assistant = 模型傳回了空物件。
task-record-parse-error = 解析錯誤：{ $kind ->
    [json] 模型回應 JSON 無效（類別 `{ $category }`），第 { $line } 行、第 { $column } 欄
    [thinking_not_allowed] 目前回應模式不接受思考輸出，第 { $line } 行、第 { $column } 欄
    [thinking_envelope_missing] 模型回應缺少規定的思考信封，第 { $line } 行、第 { $column } 欄
    [thinking_envelope_unclosed] 模型回應的思考信封未閉合，第 { $line } 行、第 { $column } 欄
    [thinking_empty] 模型回應的思考內容為空，第 { $line } 行、第 { $column } 欄
    [thinking_nested] 模型回應包含巢狀思考信封，第 { $line } 行、第 { $column } 欄
    [thinking_repeated] 模型回應包含重複思考信封，第 { $line } 行、第 { $column } 欄
    [markdown_fence_no_body] Markdown 圍欄沒有正文，第 { $line } 行、第 { $column } 欄
    [markdown_fence_unsupported] 只接受無語言標記或 json 標記的單層 Markdown 圍欄，第 { $line } 行、第 { $column } 欄
    [markdown_fence_unclosed] Markdown 圍欄未閉合，第 { $line } 行、第 { $column } 欄
   *[markdown_fence_invalid_closing] Markdown 圍欄必須在最後一個獨立行閉合，第 { $line } 行、第 { $column } 欄
}
task-record-attempt-succeeded = 嘗試 { $number }：成功；finish reason { $finish_reason }
task-record-attempt-token-usage = ；token `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ；耗時 `{ $duration }`
task-record-attempt-request-id = ；request ID { $request_id }
task-record-attempt-response-id = ；response ID { $response_id }
task-record-attempt-retryable = 嘗試 { $number }：可重試請求失敗；診斷 `{ $code }`；耗時 `{ $duration }`
task-record-attempt-retry-after = ；Retry-After `{ $duration }`
task-record-attempt-wait-retry = ；等待 `{ $duration }` 後重試
task-record-attempt-wait-completed = ；等待 `{ $duration }` 已完成，下一次嘗試未開始
task-record-attempt-wait-cancelled = ；計畫等待 `{ $duration }`，等待期間取消
task-record-attempt-failed = 嘗試 { $number }：請求或回應處理失敗；診斷 `{ $code }`；耗時 `{ $duration }`
task-record-attempt-cancelled = 嘗試 { $number }：已取消；耗時 `{ $duration }`
task-record-structured-reason = 原因：{ $reason }
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
task-record-rejected-heading = 未接受：
task-record-rejected-item = { $id }：{ $reason }
task-record-protocol-diagnostic = 協定診斷：{ $diagnostic }
task-record-unavailable-reason = 不可用原因：{ $reason }
task-record-task-diagnostic = 任務診斷：`{ $code }`；原因 { $reason }
task-record-rejection-reason = { $code ->
    [missing] 缺少模型輸出
    [duplicate] 重複模型輸出
    [invalid_shape] { $detail }
    [invalid_shape_array] 譯文必須是字串陣列
    [invalid_shape_item] 譯文陣列第 { $line } 項必須是字串
    [line_count_mismatch] 行數不符（預期 { $expected }，實際 { $actual }）
    [invalid_line_text] 第 { $line } 行包含無效控制字元
    [blank_line_mismatch] 第 { $line } 行空白狀態不符（預期{ $expected_blank ->
        [blank] 空白
       *[other] 非空白
    }）
    [blank_translation] 譯文為空
    [no_natural_language_text] 譯文沒有自然語言文字
    [contains_byte_order_mark] 譯文包含 BOM
    [placeholder_mismatch] 預留位置不符：{ $detail }
    [unexpected_placeholder] 出現未知預留位置：{ $detail }
    [placeholder_normalization_ambiguous] 預留位置正規化有歧義：{ $detail }
    [source_residual] 偵測到來源語言殘留：{ $detail }
    [tag_value_contains_closing_delimiter] 第 { $line } 行包含會提前閉合標籤值的 '>'
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason 不是 stop：{ $detail }
    [invalid_response] { $detail }
    [invalid_id] 模型第 { $index } 個項目的 ID 無效
    [unknown_id] 模型第 { $index } 個項目傳回未知 ID { $detail }
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] 無法解析模型回應
    [all_outputs_rejected] 所有模型輸出均未通過驗收
    [recoverable_request_exhausted] 可復原請求的重試額度已用盡
    [retry_after_exceeds_maximum] Retry-After 超過已設定的最長等待時間
   *[other] { $code }
}
task-record-duration-seconds = { $value } 秒
task-record-duration-milliseconds = { $value } 毫秒
