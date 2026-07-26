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
    [process_output] 處理程序輸出
    [lua] 專案 Lua 執行
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
