app-about = 使用可重用專案狀態翻譯遊戲與結構化文字
cli-test-about = 檢查發行設定和全部 LLM Client
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
cli-ownership-export-about = 匯出全部 RPG Maker 擷取項目的文字所有權
cli-translation-export-about = 匯出全部擷取項目的原文、目前譯文與狀態
cli-manual-check-about = 唯讀檢查人工譯文 TOML
cli-manual-apply-about = 套用已填寫且有效的人工譯文
cli-project-lua-about = 對專案資料庫執行 Lua 指令碼
cli-project-name-help = 穩定專案名稱
cli-init-path-help = 輸入根目錄；既有專案可重用上次成功路徑
cli-source-language-help = 原文語言 ID
cli-target-language-help = 譯文目標語言 ID
cli-builtin-help = 使用 ATT 內建的 RPG Maker 文字位置
cli-rules-help = 以此 TOML 定義取代 RPG Maker 擷取規則；空規則清單會停用規則
cli-dialogue-rules-help = 取代與 Builtin 搭配使用的 MV 對話姓名投影
cli-profile-help = 翻譯 Profile ID；省略時重用上次成功 Profile
cli-terms-help = 取代專案術語資源
cli-placeholders-help = 取代專案 Placeholder 資源
cli-project-lua-script-help = 要對專案資料庫執行的 Lua 指令碼
cli-project-lua-arguments-help = 在 -- 後傳給 Lua arg[1..] 的 UTF-8 參數
cli-manual-file-help = 人工譯文 TOML 檔案
cli-jsonl-file-help = JSONL 匯出檔案
cli-retry-rejected-help = 重新處理已儲存的 Rejected 候選
cli-manual-selection-help = 匯出範圍：pending（預設）、rejected 或 all
cli-manual-ids-help = 依 JSONL 檔案中的自然 ID 匯出項目
cli-layout-rules-help = 載入並儲存 WriteBack 排版規則 TOML
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
plan-source-explicit = 明確輸入
plan-source-project-state = 專案狀態
plan-source-product-default = 產品行為
notice-init-reuse-path = 未提供來源路徑，已沿用上次成功路徑：{ $path }。
notice-extract-reuse-owners = 未提供擷取範圍，已沿用上次成功方案：{ $owners }。
notice-translate-reuse-profile = 未提供 Profile，已沿用上次成功 Profile：{ $profile }。
notice-no-model-request = 所有翻譯單元都是最新狀態，本次不需請求模型。
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
progress-no-work = 不需要處理
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
result-translate-completed = 翻譯執行結束：{ $project }（Profile：{ $profile }）
result-translate-status = 狀態：{ $status }
result-translate-status-value = { $status ->
    [no_work] 不需要處理
    [complete] 完整
    [incomplete] 未完整
   *[other] __ATT_FALLBACK__
}
result-translate-summary = 翻譯：計畫 { $total } 個工作，已開始 { $started }，未開始 { $not_started }；完整 { $complete }，部分 { $partial }，無法使用 { $unavailable }，失敗 { $failed }，取消 { $cancelled }；寫入 { $written } 處，剩餘 { $remaining } 處，其中 Rejected { $rejected } 處
result-translate-convergence = 狀態收斂：保留 { $retained }，失效 { $invalidated }，不適用 { $not_applicable }，重用 { $reused }
result-write-back-completed = 寫回完成：{ $project }
result-project-lua-completed = 專案 Lua 執行完成：{ $project }
result-output-directory = 輸出目錄：{ $path }
result-write-back-summary = 寫回：套用譯文 { $translated } 個單元，保留原文 { $original } 個單元
result-generic-extract-unchanged = Generic 輸入未變更：{ $files } 個檔案，{ $groups } 個群組，{ $units } 個單元
result-generic-extract-updated = Generic 輸入已更新：{ $files } 個檔案，{ $groups } 個群組，{ $units } 個單元；保留 { $preserved } 條譯文，清除 { $cleared } 條
result-generic-translate-summary = Generic 翻譯：計畫 { $total } 個工作，已開始 { $started }，未開始 { $not_started }；完整 { $complete }，部分 { $partial }，無法使用 { $unavailable }，失敗 { $failed }，取消 { $cancelled }；計畫 Unit { $planned_units }，剩餘 Unit { $remaining_units }，其中 Rejected Unit { $rejected_units }，清除 { $cleared }，重用 { $reused }，接受 { $accepted }，寫入 { $written }，衝突 { $conflicted }，回應問題 { $problems }
result-generic-write-back-summary = Generic 寫回：套用譯文 { $translated } 個單元，保留原文 { $original } 個單元
result-run-log = 執行記錄：{ $path }
result-test-configuration = 設定：{ $status ->
    [passed] 通過
   *[failed] 失敗
}
result-test-client = LLM { $client }：{ $status ->
    [passed] 通過
   *[failed] 失敗
}（{ $protocol }，{ $stream ->
    [streaming] 串流
   *[non_streaming] 非串流
}）
result-test-summary = 彙總：{ $passed }/{ $total } 通過，{ $failed } 失敗，{ $skipped } 未執行
translate-incomplete-object = 專案 { $project } 的本次 Translate
translate-incomplete-rpg-maker-reason = 部分任務 { $partial }，不可用任務 { $unavailable }，未開始任務 { $not_started }，協定問題 { $protocol }，請求耗盡 { $exhausted }；請求准入{
    $admission ->
        [stopped] 已停止
       *[open] 未停止
    }；剩餘決策 { $remaining_decisions }，剩餘位置 { $remaining_locations }，其中 Rejected { $rejected_locations } 處
translate-incomplete-generic-reason = 部分任務 { $partial }，不可用任務 { $unavailable }，未開始任務 { $not_started }，請求耗盡 { $exhausted }；請求准入{
    $admission ->
        [stopped] 已停止
       *[open] 未停止
    }；剩餘 Unit { $remaining_units }，其中 Rejected Unit { $rejected_units }，寫入衝突 { $conflicted }，回應問題 { $problems }
translate-incomplete-help = 查看本次執行記錄中的具體任務診斷，修正可重現的問題後再次執行 Translate；少量剩餘內容可使用 Manual
translate-incomplete-rejected-help = 查看本次執行記錄中的具體任務診斷；Rejected 內容可用 --retry-rejected 再次翻譯，或用 manual export --selection rejected 匯出後透過 Manual 處理
result-cancelled = 命令已在安全收尾後取消。
result-plan-saved = 已儲存本次成功執行方案。
log-run-started = 命令 { $command } 已開始。
log-run-succeeded = 命令 { $command } 已成功完成。
log-run-failed = 命令 { $command } 失敗。
log-run-outcome-unknown = 命令 { $command } 已結束，但最終結果未知；請先依診斷處理，再決定是否重試。
log-run-cancelled = 命令 { $command } 已取消。
log-performance-counters = 效能計數：SQLite 事務控制嘗試 { $sqlite_control_attempted_total } 次；完整候選樹驗證開始 { $candidate_validation_started } 次，完成 { $candidate_validation_completed } 次。
log-lua-print = Lua：{ $message }
log-plan-resolved = 命令 { $command } 的方案來自{ $source }。
log-phase-started = 階段開始：{ $phase }。
log-retry-summary = 共執行 { $count } 次重試。
log-translation-task-started = 翻譯工作 { $index }/{ $total } 已開始。
log-translation-task-finished = 翻譯工作 { $index } 已結束，結果為 { $outcome }。{ $provider_status ->
    [present] 上游服務方：{ $provider }。
   *[missing] 上游服務方：未提供。
}
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
log-phase-name = { $phase ->
    [check_project] 專案檢查
    [scan_source] 來源檔案掃描
    [prepare_candidate] 候選建構
    [update_database] 資料庫更新
    [publish] 發佈
    [builtin] Builtin 擷取
    [builtin_documents] Builtin 文件掃描
    [builtin_work_units] Builtin 文字單元擷取
    [builtin_commit] Builtin 提交
    [rules] Rules 擷取
    [rules_documents] Rules 文件掃描
    [rules_matches] Rules 比對
    [rules_commit] Rules 提交
    [lua] Lua 執行
    [planning] 翻譯任務規劃
    [confirmed_tasks] 翻譯任務確認
    [read_assets] 專案內容讀取
    [plan_rpg_maker_write_back] 寫回規劃
    [rewrite_documents] 文件改寫
    [validate_candidate] 候選驗證
   *[other] __ATT_FALLBACK__
}
log-task-outcome-value = { $outcome ->
    [complete] 完成
    [partial] 部分完成
    [unavailable] 暫時無法使用
    [failed] 失敗
    [not_committed_after_earlier_failure] 因先前失敗未提交
    [cancelled] 已取消
   *[other] 結果無法辨識
}
diagnostic-object = 對象：{ $subject }
diagnostic-error-heading = 錯誤：
diagnostic-warning-heading = 警告：
diagnostic-explanation = 原因：{ $reason }
diagnostic-impact = 影響：{ $impact }
diagnostic-resolution = 處理方式：{ $action }
diagnostic-related = { $relation ->
    [cleanup] 同時，清理失敗：
    [rollback] 同時，回復失敗：
    [discard] 同時，捨棄候選失敗：
    [finalization] 同時，收尾失敗：
    [shutdown] 同時，關閉失敗：
    [observability] 同時，結果呈現或記錄失敗：
   *[other] 同時，相關操作失敗：
}
diagnostic-impact-value = { $effect ->
    [unchanged] 業務狀態沒有修改
    [progress_preserved] 先前確認的進度仍然保留；指出的內容沒有完成
    [applied] 相關業務結果已經生效
    [applied_run_plan_not_saved] 業務結果已經生效，但本次執行方案沒有儲存
    [applied_finalization_failed] 業務結果已經生效，但必要收尾沒有完成
    [recovery_required] 結果已經明確，但必須先處理指出的復原現場
    [outcome_unknown] 無法確認本次操作是否生效；依處理方式復原前不要重試或刪除現場
   *[other] __ATT_FALLBACK__
}
diagnostic-resolution-value = { $code ->
    [fix_configuration] 修正指出的設定欄位後重試
    [fix_input] 修正指出的輸入後重試
    [fix_placeholder_rules] 修正指出的 Placeholder 規則後重試
    [review_translation] 複核指出的譯文；需要修正時使用 Manual
    [review_disabled_rules] 如果這是預期結果，無需處理；否則在指出的檔案中加入有效規則並重新執行 Extract
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
    [empty_text_capture] text 擷取為空
    [rules_owner_disabled] 選擇的 Rules 檔案使用 rule = []；Rules 已停用，並已刪除其擷取資產
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
    [stdout_write_failed] 無法寫入標準輸出
    [stderr_write_failed] 無法寫入標準錯誤
    [stdout_flush_failed] 無法重新整理標準輸出
    [stderr_flush_failed] 無法重新整理標準錯誤
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
    [not_regular_file] 現有目標不是一般檔案
    [wrong_publisher_instance] 發佈權杖屬於另一個發佈器執行個體
    [journal_corrupt] 發佈復原日誌無效或不完整
    [unexpected_artifact] 非預期的檔案系統產物阻擋了作業
    [interactive_session_already_open] 另一個互動式 SQLite 工作階段已在執行
    [backup_incomplete] SQLite 備份未達到完成狀態
    [request_serialization_failed] 無法序列化模型請求
    [http_client_build_failed] 無法建立模型服務 HTTP Client
    [dns_resolution_failed] DNS 解析失敗
    [tcp_connection_failed] TCP 連線失敗
    [request_send_failed] HTTP 請求傳送失敗
    [response_read_failed] HTTP 回應讀取失敗
    [tls_handshake_failed] TLS 握手失敗
    [connect_timed_out] TCP 連線逾時
    [read_timed_out] HTTP 回應讀取逾時
    [request_timed_out] HTTP 請求超過總逾時
    [response_decode_failed] HTTP 回應解碼失敗
    [redirect_rejected] HTTP 重新導向遭拒
    [response_parsing_failed] 模型回應不是有效的 JSON
    [model_stream_invalid_json] 模型串流事件不是有效的 JSON
    [model_stream_invalid_utf8] 模型串流包含無效的 UTF-8
    [model_stream_error_event] 模型串流傳回服務錯誤事件
    [model_stream_unclosed_event] 模型串流的 SSE 事件未以空行結束
    [model_stream_missing_finish] Chat 模型串流缺少 finish_reason
    [model_stream_missing_responses_terminal] Responses 模型串流缺少終態事件
    [model_stream_event_type_mismatch] 模型串流的 SSE event 與 JSON type 不一致
    [model_stream_duplicate_choice] 模型串流重複傳回同一 choice
    [model_stream_choice_after_finish] Chat 模型串流在 finish 後又傳送了會改變回應的欄位
    [model_stream_unexpected_done] Responses 模型串流意外傳回 [DONE]
    [response_json_invalid] Assistant 回應不是有效的 JSON
    [response_shape_invalid] Assistant JSON 的根結構或回應結構不符合要求
    [response_id_invalid] 回應項目的 output ID 無效
    [response_id_unexpected] 回應包含本任務未要求的 output ID
    [response_id_duplicate] 回應重複傳回了同一個 output ID
    [response_id_missing] 回應缺少本任務要求的 output ID
    [response_translation_not_array] translation 必須是字串陣列
    [response_translation_item_not_string] translation 陣列中存在非字串項目
    [response_echo_shape_invalid] 回顯的 source 物件不符合要求的 source/translation 結構
    [response_echo_source_item_not_string] 回顯的 source 陣列中存在非字串項目
    [response_translation_blank] 傳回的譯文為空
    [response_translation_text_invalid] 傳回的譯文包含不允許的換行、NUL 或位元組順序標記
    [response_placeholder_snapshot_invalid] 用於驗收回應的 Placeholder 快照無效
    [response_placeholder_identity_or_count_mismatch] 譯文改變了必要 Placeholder 的身分或數量
    [response_placeholder_missing] 譯文缺少必要的控制 token
    [response_placeholder_unexpected] 譯文包含計畫外的控制 token
    [response_placeholder_order_mismatch] 譯文改變了必要控制 token 的順序
    [response_placeholder_binding_mismatch] 譯文改變了必要 Placeholder 與正文的綁定關係
    [response_placeholder_boundary_mismatch] 譯文新增或刪除了必要的 Placeholder 邊界
    [response_placeholder_reserved_token] 譯文包含保留的 Placeholder token
    [response_placeholder_ambiguous] 傳回的 Placeholder 無法唯一對應到所需 token
    [response_control_token_invalid] 傳回的控制 token 結構無效
    [response_text_segment_count_mismatch] 回應改變了必要的文字片段數量
    [response_text_segment_shape_mismatch] 回應改變了必要的文字片段結構
    [response_line_count_mismatch] translation 陣列項目數與要求不符
    [response_line_text_invalid] translation 陣列項目包含無法驗收的文字
    [response_blank_line_mismatch] translation 陣列未保持必要的空槽與非空槽位置
    [response_source_residual] 已接受的譯文仍含來源語言文字，需要複核
    [response_finish_requires_review] 模型因非最終原因停止；傳回結果需要複核
    [response_thinking_empty] 必填的 think 欄位為空或僅含空白
    [response_no_usable_output] Assistant 回應沒有可用輸出
    [response_all_outputs_rejected] Assistant 回應中的全部輸出都未通過驗收
    [invalid_response_contract] 模型回應不符合必要的回應契約
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
diagnostic-http-route-direct = 直接連線（未使用代理）
diagnostic-http-route-proxy = 透過明確代理 { $proxy }
diagnostic-retry-after = Retry-After：{ $seconds } 秒
diagnostic-provider-code = 服務方 code：{ $code }
diagnostic-provider-type = 服務方 type：{ $kind }
diagnostic-provider-message = 服務方訊息：{ $message }
diagnostic-json-position = 第 { $line } 行，第 { $column } 欄
diagnostic-input-field = 欄位：{ $field }
diagnostic-input-failure = { $code ->
    [syntax] TOML 語法無效
    [missing_field] 缺少必填欄位
    [unknown_field] 目前格式不接受此欄位
    [duplicate_field] 欄位重複
    [type_mismatch] 欄位類型不符要求
    [invalid_value] 欄位值不符要求
   *[other] __ATT_FALLBACK__
}
diagnostic-expected-type = 要求的類型：{ $expected ->
    [string] 字串
    [integer] 整數
    [boolean] 布林值
    [string_or_boolean] 字串或布林值
    [string_array] 字串陣列
    [integer_array] 整數陣列
    [table] 資料表
    [table_array] 資料表陣列
    [array] 陣列
    [object] 物件
   *[other] __ATT_FALLBACK__
}
diagnostic-response-item = 回應第 { $item } 項
diagnostic-array-item = 陣列第 { $item } 項
diagnostic-token-position = 控制 token 第 { $position } 個位置
diagnostic-text-segment = 文字第 { $segment } 個片段
diagnostic-post-finish-fields = finish 後的欄位：{ $fields }
diagnostic-expected-actual = 要求 { $expected }，實際 { $actual }
diagnostic-placeholder-rule-file = { $path } 中的 Placeholder 規則 { $number }
diagnostic-placeholder-rule-project = 目前專案的 Placeholder 規則 { $number }
manual-exported = 已匯出 { $entries } 筆：{ $path }
manual-checked = 有效 { $valid }，未填寫 { $unfilled }，錯誤 { $errors }
manual-applied = 已套用 { $applied }，未填寫 { $unfilled }，錯誤 { $errors }
manual-value = { $code ->
    [translation_byte_order_mark] translation 第 { $line } 項包含 BOM（U+FEFF）
    [remove_byte_order_mark] 刪除譯文中的 BOM（U+FEFF）字元
    [keep_placeholders] 在譯文中還原原文的 Placeholder，保留其數量、要求的順序及所屬文字槽
    [invalid_source_line] source 第 { $line } 項包含換行或 NUL
    [invalid_translation_line] translation 第 { $line } 項包含換行或 NUL
    [fixed_length] fixed 譯文需要 { $expected } 項，目前為 { $actual } 項
    [fixed_blank_slot] fixed 譯文第 { $line } 項必須保留空槽
    [rerun_export] 重新執行 manual export
    [rerun_export_without_controls] 重新執行 manual export，不要把換行或 NUL 寫進陣列項目
    [rerun_export_then_fill] 重新執行 manual export 後再填寫譯文
    [resolve_temporary_then_rerun_export] 處理顯示的固定暫存路徑；如有遺留物件，請將其移除，然後重新執行 manual export
    [resolve_published_backup_cleanup] 兩份匯出已經生效；確認輸出後刪除顯示的固定 backup 檔案
    [keep_exported_type] 保留 manual export 產生的 type
   *[other] __ATT_FALLBACK__
}
task-record-title = 翻譯任務
task-record-final-result-heading = 最終結果
task-record-final-status = 狀態：{ $state ->
    [complete] 完成，已確認提交
    [partial] 部分完成，已確認提交
    [unavailable_rejected_committed] 無法使用，已儲存拒絕候選
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
task-record-provider = 上游服務方：{ $provider }
task-record-provider-unavailable = 上游服務方：未提供
task-record-requested = 要求譯文：{ $requested } 項
task-record-accepted-written = 已接受：{ $accepted } 項（ID：{ $ids }），寫入 { $written } 個實際位置
task-record-accepted-outcome-unknown = 已驗收：{ $accepted } 項（ID：{ $ids }）；無法確認資料庫提交終態
task-record-unaccepted = 未接受：{ $unaccepted } 項（ID：{ $ids }）
task-record-task-diagnostic = 任務診斷
