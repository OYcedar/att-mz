app-about = 使用可复用项目状态翻译游戏和结构化文本
cli-ui-language-help = Help、诊断、进度、结果和项目日志使用的语言：ar、zh-Hans、zh-Hant、en、fr、ru、es、ja、ko 或 vi
cli-progress-help = 实时进度模式：auto、plain 或 off
cli-mz-about = 翻译 RPG Maker MZ 游戏
cli-mv-about = 翻译 RPG Maker MV 游戏
cli-generic-about = 翻译约定 JSONL 文本
cli-init-about = 初始化或更新一个命名翻译项目
cli-extract-about = 从项目当前输入同步原文
cli-translate-about = 使用显式或已保存的 Profile 翻译已提取原文
cli-write-back-about = 将当前译文写入项目输出
cli-project-lua-about = 在项目中一次性运行原子数据库 Lua
cli-project-name-help = 稳定项目名称
cli-init-path-help = 输入根目录；已有项目可复用上次成功路径
cli-source-language-help = 原文语言 ID
cli-target-language-help = 译文目标语言 ID
cli-dialogue-width-help = 对话正文每行允许的最大全角字符数
cli-scrolling-width-help = 滚动文本每行允许的最大全角字符数
cli-help-width-help = 帮助或说明框每行允许的最大全角字符数
cli-builtin-help = 使用 ATT 内置的 RPG Maker 文本位置
cli-rules-help = 用该 TOML 定义替换 RPG Maker 提取规则；空规则列表会停用规则
cli-dialogue-rules-help = 替换与 Builtin 配合使用的 MV 对话姓名投影
cli-profile-help = 翻译 Profile ID；省略时复用上次成功 Profile
cli-terms-help = 替换项目术语资源
cli-placeholders-help = 替换项目 Placeholder 资源
cli-project-lua-script-help = 本次一次性运行的原子数据库 Lua 程序
cli-project-lua-arguments-help = 在 -- 后传给 Lua arg[1..] 的 UTF-8 参数
cli-usage-heading = 用法：
cli-commands-heading = 命令：
cli-options-heading = 选项：
cli-arguments-heading = 参数：
cli-options-metavar = 选项
cli-command-metavar = 命令
cli-print-help = 显示帮助
cli-print-version = 显示版本
cli-blank-value = 值不能为空。
cli-invalid-positive-integer = 值必须是正整数。
cli-invalid-progress = 不支持进度模式 { $value }；请使用 auto、plain 或 off。
cli-invalid-ui-language-argument = --ui-language 包含无效语言标签：{ $value }。
cli-unsupported-ui-language-argument = --ui-language 指定了不支持的语言：{ $value }。
cli-invalid-ui-language-environment = ATT_UI_LANGUAGE 包含无效语言标签：{ $value }。
cli-unsupported-ui-language-environment = ATT_UI_LANGUAGE 指定了不支持的语言：{ $value }。
cli-ui-language-environment-not-unicode = ATT_UI_LANGUAGE 不是有效 Unicode。
cli-unexpected-argument = 未预期的参数：{ $value }。
cli-missing-required-argument = 缺少必需参数：{ $value }。
cli-invalid-value = { $argument } 的值 { $value } 无效。
cli-error-heading = 错误：
cli-try-help = 如需更多信息，请使用 --help。
cli-missing-value = { $argument } 需要提供值。
cli-missing-subcommand = 必须提供一个命令。
cli-argument-conflict = { $argument } 不能与当前其他参数同时使用。
cli-wrong-number-of-values = { $argument } 的值数量不正确。
cli-invalid-utf8 = 命令行参数不是有效 Unicode。
cli-parse-failure = 无法解析命令行。
error-no-executable-extract-owner = 清除后没有可执行的 Extract owner，因此未保存方案。
plan-source-explicit = 显式输入
plan-source-project-state = 项目状态
plan-source-product-default = 产品行为
notice-init-reuse-path = 未提供来源路径，已沿用上次成功路径：{ $path }。
notice-extract-reuse-owners = 未提供提取范围，已沿用上次成功方案：{ $owners }。
notice-translate-reuse-profile = 未提供 Profile，已沿用上次成功 Profile：{ $profile }。
notice-owner-disabled = 已停用 owner { $owner }，并将其移出后续自动方案。
warning-rules-command-non-string-skipped = 警告：Rules 规则 { $rule_number } 跳过了 { $skipped_count } 个非字符串 command 参数（来源 { $source_file }，code={ $command_code }，parameter={ $parameter }，类型 { $actual_type }）。
warning-manual-layout-required = 警告：以下文本需要人工检查换行：{ $locations }（区域={ $region }，全角字符上限={ $max_fullwidth_chars }）。
notice-no-model-request = 全部翻译单元均为最新状态，本次无需请求模型。
notice-manual-layout = 有 { $count } 个单元需要人工检查换行。
notice-log-degraded = 项目日志不可用或已降级；命令会继续，退出状态不受影响。
notice-task-records-degraded = 翻译任务记录不可用或已降级；命令会继续，退出状态不受影响。
progress-init-check-project = 正在检查项目状态
progress-init-scan-source = 正在扫描游戏来源
progress-init-build-candidate = 正在构建项目候选
progress-init-converge-database = 正在收敛项目数据库
progress-init-publish = 正在发布初始化项目
progress-save-run-plan = 正在保存成功运行方案
progress-extract-owner = 提取 owner：{ $owner }
progress-extract-documents = 正在扫描文档
progress-extract-builtin = Builtin 工作单元
progress-extract-rules = Rules 规则
progress-extract-commit = 正在提交提取资产
progress-generic-init = 正在初始化 Generic 项目
progress-generic-extract = 正在扫描 Generic JSONL 输入
progress-translate-planning = 正在规划翻译任务
progress-translate-confirmed = 已确认翻译任务
progress-translate-no-work = 无需调用模型
progress-project-lua = 正在运行项目 Lua 程序
progress-write-back-read-assets = 正在读取已验收资产
progress-write-back-planning = 正在规划文档改写
progress-write-back-documents = 已改写文档
progress-write-back-validate-candidate = 正在验证输出候选
progress-write-back-publish = 正在发布输出；中断后会等待明确终态
progress-finalizing = 正在完成必要收尾
progress-safe-stopping = 正在安全停止；保留最后确认进度
result-init-completed = 初始化完成：{ $project }
result-init-created = 项目状态：已创建
result-init-unchanged = 项目状态：无变化
result-init-updated = 项目状态：已更新
result-init-stale-owners = 需重新提取：{ $owners }
result-extract-completed = 提取完成：{ $project }
result-translate-completed = 翻译执行完成：{ $project }（Profile：{ $profile }）
result-translate-summary = 翻译：任务 { $total }，完整 { $complete }，部分 { $partial }，不可用 { $unavailable }；写入 { $written } 处，剩余 { $remaining } 处
result-translate-convergence = 状态收敛：保留 { $retained }，失效 { $invalidated }，不适用 { $not_applicable }，复用 { $reused }
result-write-back-completed = 写回完成：{ $project }
result-project-lua-completed = 项目 Lua 执行完成：{ $project }
result-output-directory = 输出目录：{ $path }
result-write-back-summary = 写回：应用译文 { $translated } 个单元，保留原文 { $original } 个单元；自动换行 { $auto_wrapped } 段，新增换行 { $breaks } 处；续行全角缩进 { $indents } 处；需人工换行 { $manual } 段
result-generic-extract-unchanged = Generic 输入未变化：{ $files } 个文件，{ $groups } 个组，{ $units } 个单元
result-generic-extract-updated = Generic 输入已更新：{ $files } 个文件，{ $groups } 个组，{ $units } 个单元；保留 { $preserved } 条译文，清除 { $cleared } 条
result-generic-translate-summary = Generic 翻译：任务 { $total }，完整 { $complete }，部分 { $partial }，不可用 { $unavailable }；清除 { $cleared }，复用 { $reused }，接受 { $accepted }，写入 { $written }，冲突 { $conflicted }，响应问题 { $problems }
result-generic-write-back-summary = Generic 写回：应用译文 { $translated } 个单元，保留原文 { $original } 个单元
result-cancelled = 命令已在安全收尾后取消。
result-plan-saved = 已保存本次成功运行方案。
log-run-started = 命令 { $command } 已开始。
log-run-succeeded = 命令 { $command } 已成功完成。
log-run-failed = 命令 { $command } 失败。
log-run-outcome-unknown = 命令 { $command } 结束，但最终结果未知；请按错误中的恢复位置处理。
log-run-cancelled = 命令 { $command } 已取消。
log-performance-counters = 性能计数：SQLite 事务控制尝试 { $sqlite_control_attempted_total } 次；完整候选树校验开始 { $candidate_validation_started } 次，完成 { $candidate_validation_completed } 次。
log-lua-script = Lua 脚本 { $identity }（SHA-256 { $fingerprint }）。
log-lua-print = Lua：{ $message }
log-lua-summary = Lua 统计：数据库调用 { $database_calls } 次，修改行 { $changed_rows } 行，译文调用 { $translation_calls } 次，print { $printed_lines } 行。
log-plan-resolved = 命令 { $command } 的方案来自{ $source }。
log-phase-started = 阶段开始：{ $phase }。
log-retry-summary = 共执行 { $count } 次重试。
log-translation-task-started = 翻译任务 { $index }/{ $total } 已开始。
log-translation-task-finished = 翻译任务 { $index } 已结束，结果为 { $outcome }。
log-run-recovery-required = 命令 { $command } 结束时需要恢复；请按诊断中的恢复位置处理。
log-phase-completed = 阶段已完成：{ $phase }。
log-phase-stopped = { $outcome ->
    [failed] 阶段失败：{ $phase }。
    [cancelled] 阶段已取消：{ $phase }。
   *[other] 阶段已停止：{ $phase }。
}
log-cancellation-requested = 已请求取消；已确认 { $confirmed }/{ $total } 项。
log-cancellation-requested-indeterminate = 已请求取消；已确认 { $confirmed } 项，总数未知。
log-run-plan-finalized = { $result ->
    [saved] 运行计划已保存。
    [not_saved] 运行计划未保存。
    [saved_finalization_failed] 运行计划已保存，但收尾失败。
    [outcome_unknown] 运行计划的最终状态未知。
   *[other] 运行计划收尾停止，结果无法识别。
}
log-translation-finished = { $result ->
    [not_started] 翻译未开始。
    [no_work] 翻译结束，没有需要处理的内容。
    [complete] 翻译已完成。
    [incomplete] 翻译结束，但仍有未完成内容。
    [failed] 翻译失败。
    [cancelled] 翻译已取消。
   *[other] 翻译已停止，结果无法识别。
}
log-publication-started = 开始发布到输出根目录 { $path }。
log-publication-finished = { $result ->
    [published] 发布已完成。
    [not_published] 发布未修改输出。
    [recovery_required] 发布已停止，需要恢复。
    [outcome_unknown] 发布的最终状态未知。
   *[other] 发布已停止，结果无法识别。
}
log-project-log-degraded = 项目日志发生故障；已记录 { $failure_kinds } 类故障。
log-task-outcome-value = { $outcome ->
    [complete] 完成
    [partial] 部分完成
    [unavailable] 暂不可用
    [failed] 失败
    [not_committed_after_earlier_failure] 因前序失败未提交
    [cancelled] 已取消
   *[other] 结果无法识别
}
diagnostic-title = 错误 [{ $code }]
diagnostic-stage = 阶段：{ $stage }
diagnostic-location = 位置：{ $subject }
diagnostic-explanation = 原因：{ $reason }
diagnostic-effect = 影响：{ $impact }
diagnostic-resolution = 处理办法：{ $action }
diagnostic-related = 相关错误 { $index }：
diagnostic-relation-value = { $code ->
    [cleanup] 清理
    [rollback] 回滚
    [discard] 丢弃
    [finalization] 收尾
    [shutdown] 关闭
    [observability] 可观测性
   *[other] { $code }
}
diagnostic-stage-value = { $code ->
    [process_startup] 进程启动
    [process_output] 进程输出
    [configuration] 配置加载
    [command_preparation] 命令准备
    [project_opening] 项目打开
    [init] 初始化
    [extract] 提取
    [translate] 翻译
    [write_back] 写回
    [lua] 项目 Lua 执行
    [model_request] 模型请求
    [run_plan_finalization] 运行方案收尾
    [publication] 发布
    [shutdown] 关闭
    [logging] 项目日志
    [runtime] 运行时
   *[other] __ATT_FALLBACK__
}
diagnostic-effect-value = { $code ->
    [unchanged] 状态未改变
    [progress_preserved] 已保留有效进度
    [applied] 状态已生效
    [applied_run_plan_not_saved] 状态已生效，但运行方案未保存
    [applied_finalization_failed] 状态已生效，但收尾未完成
    [recovery_required] 必须先恢复，才能信任当前状态
    [outcome_unknown] 最终状态未知
   *[other] __ATT_FALLBACK__
}
diagnostic-resolution-value = { $code ->
    [fix_configuration] 修正指出的配置字段后重试
    [fix_input] 修正指出的输入后重试
    [fix_placeholder_rules] 修正指出的 Placeholder 规则后重试
    [adjust_manual_layout] 按指出的位置和显示宽度人工调整换行与布局
    [check_path_and_permissions] 检查路径、文件系统状态和权限
    [check_project_state] 检查并修正项目状态后重试
    [resolve_contention] 等待冲突操作结束后重试
    [check_model_service] 检查模型服务响应和账户配额
    [preserve_recovery_artifacts] 不要删除列出的恢复产物；先恢复输出，再重试
    [retry] 重试该操作
    [report_bug] 携带错误码和日志路径报告 ATT 缺陷
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] 缺少必填值
    [generic_extract_required] 当前 JSONL 输入与最近一次 Extract 不一致；请重新运行 att generic extract
    [conflicting_values] 提供的值互相冲突
    [invalid_syntax] 值的语法无效
    [invalid_encoding] 文本编码无效
    [invalid_value] 值不符合要求的契约
    [not_found] 所需对象不存在
    [state_mismatch] 已保存的项目状态不满足本次操作
    [unsupported_windows_code_page] Windows 代码页不是 UTF-8
    [transaction_rolled_back] 事务失败，改动已回滚
    [transaction_outcome_unknown] 事务结束时无法确认提交或回滚结果
    [finalization_failed] 操作结果已经产生，但收尾失败
    [rollback_failed] 主操作失败，并且回滚也失败
    [external_service_rejected] 外部服务拒绝了请求
    [external_service_unavailable] 外部服务当前不可用
    [executor_closed] 执行服务正在关闭或已经关闭
    [concurrent_shutdown] 另一个调用方正在关闭执行器
    [executor_state_poisoned] 执行器生命周期状态已经损坏
    [worker_spawn_failed] 操作系统无法创建工作线程
    [worker_channel_closed] 工作线程命令通道在收尾完成前关闭
    [worker_panicked] 工作线程异常终止
    [reparse_point_forbidden] 路径包含不能信任的重解析点
    [non_local_volume] 路径不在本地固定磁盘上
    [non_ntfs_volume] 路径不在 NTFS 卷上
    [case_sensitive_directory] 目录启用了区分大小写的名称语义
    [lock_cancelled] 等待所需锁时操作被取消
    [target_already_exists] 目标已经存在
    [file_identity_changed] 操作期间文件物理身份发生变化
    [invalid_path] 路径不是该操作的有效目标
    [wrong_publisher_instance] 发布令牌属于另一个发布器实例
    [journal_corrupt] 发布恢复日志损坏或不完整
    [unexpected_artifact] 意外的文件系统产物阻塞了操作
    [interactive_session_already_open] 已有 SQLite 交互会话处于活动状态
    [backup_incomplete] SQLite 备份没有完成
    [request_serialization_failed] 无法序列化模型请求
    [response_parsing_failed] 模型响应不是有效 JSON
    [invalid_response_contract] 模型响应不符合所需响应契约
    [transport_failed] 收到有效响应前 HTTP 传输失败
    [lua_compilation_failed] Lua 主程序编译失败
    [lua_execution_failed] Lua 主程序运行失败
    [rules_pattern_match_failed] 无法执行 Rules 的 PCRE2 模式
    [rules_zero_width_match] Rules 模式产生了零宽匹配
    [rules_overlapping_capture] Rules 模式产生了相互重叠的文本捕获
    [rules_missing_text_capture] 必需的命名文本捕获没有参与匹配
    [rules_invalid_capture_range] Rules 匹配或捕获范围不在有效 UTF-8 字符边界上
    [write_back_candidate_invalid] 写回候选不符合所需的 data/js 目录结构
    [write_back_recovery_required] 必须先恢复输出目录，才能信任其中内容
    [already_exists] 目标对象已存在
    [cancelled] 操作已取消
    [concurrent_modification] 项目状态在操作期间被并发修改
    [duplicate_identifier] 标识符重复
    [extraction_out_of_date] 已保存的提取结果不再匹配当前源文件
    [invalid_content] 内容不符合必需契约
    [manual_layout_required] 需要人工调整换行或布局
    [operation_failed] 操作失败
    [placeholder_projection_failed] Placeholder 投影未保留必需结构
    [profile_not_found] 所选翻译 Profile 不存在
    [recovery_required] 必须先完成恢复，才能信任该结果
    [resource_limit] 已达到所需资源限制
    [resource_limit_exceeded] 操作超出后端资源限制
    [source_snapshot_mismatch] 源文件不再匹配已保存快照
    [unavailable] 请求的工作暂时不可用
    [internal_invariant] 内部不变量被破坏；这是 ATT 缺陷
   *[other] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [language_policy_term_blank] 语言策略术语不能为空
    [language_policy_term_surrounding_whitespace] 语言策略术语不能带首尾空白
    [language_policy_term_duplicate] 语言策略术语不能重复
    [quote_repair_candidates_empty] 引号修复候选列表不能为空
    [quote_repair_delimiter_invalid] 引号修复分隔符不能是字母数字、空白或控制字符
    [quote_repair_pair_duplicate] 引号修复对不能重复
    [quote_repair_delimiter_ambiguous] 引号修复分隔符必须只属于一个配对
    [language_id_blank] 语言 ID 不能为空
    [language_id_surrounding_whitespace] 语言 ID 不能带首尾空白
    [language_id_uses_underscore] 语言 ID 的子标签之间必须使用连字符
    [language_id_invalid_syntax] 语言 ID 必须符合 RFC 5646 语法
    [language_id_invalid_registry_tag] 语言 ID 包含无效的注册表子标签
    [language_id_canonicalization_failed] 语言 ID 无法规范化
    [language_id_undefined_primary_language] 语言 ID 必须定义主语言
    [language_id_duplicate] 语言 ID 必须唯一
    [language_catalog_empty] 至少需要一个来源语言模块
    [url_invalid] 值必须是有效 URL
    [url_credentials_forbidden] URL 不能包含凭据
    [url_fragment_forbidden] URL 不能包含片段
    [url_scheme_unsupported] URL scheme 必须是 http 或 https
    [api_key_blank] API key 不能为空
    [api_key_surrounding_whitespace] API key 不能带首尾空白
    [api_key_invalid_header] API key 不是有效 HTTP Header 值
    [strict_json_invalid] 值必须是严格 JSON（行={ $line }，列={ $column }）
    [json_object_required] 值必须是 JSON 对象
    [reserved_request_field] 该字段由请求协议拥有，不能覆盖
    [proxy_must_be_false_or_url] proxy 必须是 false 或完整的 http/https URL
    [pem_path_duplicate] PEM 路径必须唯一
    [runtime_maximum_exceeded] 值超过运行时上限（实际值={ $actual }，上限={ $maximum }）
    [value_surrounding_whitespace] 值不能带首尾空白
    [value_blank] 值不能为空
    [path_blank] 路径不能为空
    [positive_required] 值必须大于零（实际值={ $actual }）
    [usize_range_exceeded] 值超过当前平台的 usize 范围（实际值={ $actual }）
    [u32_range_exceeded] 值超过 u32 范围（实际值={ $actual }）
    [duplicate_profile_id] 翻译 Profile ID 必须唯一
    [selected_profile_invalid] 所选翻译 Profile 的结构或字段类型无效
    [referenced_client_not_found] 引用的 LLM Client 不存在
   *[other] __ATT_FALLBACK__
}
task-record-title = 翻译任务 { $ordinal } · { $state }
task-record-state-label = { $state ->
    [complete] 完成
    [partial] 部分完成
    [unavailable] 不可用
    [execution_failed] 执行失败
    [commit_preparation_failed] 提交准备失败
    [commit_not_applied] 提交未应用
    [commit_outcome_unknown] 提交结果未知
    [not_committed_after_earlier_failure] 因前序失败未提交
    [invalid_result] 执行结果序列无效
    [cancelled] 已取消
   *[other] { $state }
}
task-record-summary-with-written = `任务 { $ordinal }/{ $total }` · `尝试 { $attempts } 次` · `验收 { $accepted }/{ $expected }` · `写入 { $written } 处`
task-record-summary-without-written = `任务 { $ordinal }/{ $total }` · `尝试 { $attempts } 次` · `验收 { $accepted }/{ $expected }`
task-record-run-id-label = Run ID：
task-record-started-at-label = 开始时间：
task-record-duration-label = 总耗时：
task-record-endpoint-label = Endpoint：
task-record-model-label = Model：
task-record-custom-parameters-heading = 自定义参数
task-record-attempts-heading = 请求过程
task-record-final-result-heading = 最终结果
task-record-no-request = 没有形成可发送的模型请求。
task-record-empty-assistant = 模型返回了空对象。
task-record-parse-error = 解析错误：{ $kind ->
    [thinking_empty] 模型响应的思考内容为空，第 { $line } 行、第 { $column } 列
   *[json] 模型响应 JSON 无效（类别 `{ $category }`），第 { $line } 行、第 { $column } 列
}
task-record-attempt-succeeded = 尝试 { $number }：成功；finish reason { $finish_reason }
task-record-attempt-token-usage = ；token `{ $prompt } / { $completion } / { $total }`
task-record-attempt-duration = ；耗时 `{ $duration }`
task-record-attempt-request-id = ；request ID { $request_id }
task-record-attempt-response-id = ；response ID { $response_id }
task-record-attempt-retryable = 尝试 { $number }：可重试请求失败；诊断 `{ $code }`；耗时 `{ $duration }`
task-record-attempt-retry-after = ；Retry-After `{ $duration }`
task-record-attempt-wait-retry = ；等待 `{ $duration }` 后重试
task-record-attempt-wait-completed = ；等待 `{ $duration }` 已完成，下一次尝试未开始
task-record-attempt-wait-cancelled = ；计划等待 `{ $duration }`，等待期间取消
task-record-attempt-failed = 尝试 { $number }：请求或响应处理失败；诊断 `{ $code }`；耗时 `{ $duration }`
task-record-attempt-cancelled = 尝试 { $number }：已取消；耗时 `{ $duration }`
task-record-structured-reason = 原因：{ $reason }
task-record-final-status = 状态：{ $state ->
    [complete] 完成，已确认提交
    [partial] 部分完成，已确认提交
    [unavailable] 不可用，项目未改变
    [execution_failed] 执行失败，未提交
    [commit_preparation_failed] 提交准备失败，确定未应用
    [commit_not_applied] 事务确定未应用
    [commit_outcome_unknown] 提交结果未知
    [not_committed_after_earlier_failure] 因前序任务失败而未提交
    [invalid_result] Executor 结果序列无效，未提交
    [cancelled] 已取消，未提交
   *[other] { $state }
}
task-record-accepted-written = 已接受：{ $accepted } 项，写入 { $written } 个实际位置
task-record-accepted-outcome-unknown = 已验收：{ $accepted } 项；数据库提交终态无法确认
task-record-task-diagnostic = 任务诊断：`{ $code }`；原因 { $reason }
task-record-duration-seconds = { $value } 秒
task-record-duration-milliseconds = { $value } 毫秒
