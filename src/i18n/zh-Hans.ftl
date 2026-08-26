app-about = 使用可复用项目状态翻译游戏和结构化文本
cli-ui-language-help = Help、诊断、进度、结果和项目日志使用的语言：ar、zh-Hans、zh-Hant、en、fr、ru、es、ja、ko 或 vi
cli-mz-about = 翻译 RPG Maker MZ 游戏
cli-mv-about = 翻译 RPG Maker MV 游戏
cli-generic-about = 翻译约定 JSONL 文本
cli-init-about = 初始化或更新一个命名翻译项目
cli-extract-about = 从项目当前输入同步原文
cli-translate-about = 使用显式或已保存的 Profile 翻译已提取原文
cli-write-back-about = 将当前译文写入项目输出
cli-manual-about = 使用可编辑 TOML 管理人工译文
cli-manual-export-about = 导出当前需要人工补译的条目
cli-ownership-export-about = 导出全部 RPG Maker 提取条目的文字所有权
cli-translation-export-about = 导出全部提取条目的原文、当前译文和状态
cli-manual-check-about = 只读检查人工译文 TOML
cli-manual-apply-about = 应用已填写且有效的人工译文
cli-project-lua-about = 对项目数据库运行 Lua 脚本
cli-project-name-help = 稳定项目名称
cli-init-path-help = 输入根目录；已有项目可复用上次成功路径
cli-source-language-help = 原文语言 ID
cli-target-language-help = 译文目标语言 ID
cli-builtin-help = 使用 ATT 内置的 RPG Maker 文本位置
cli-rules-help = 用该 TOML 定义替换 RPG Maker 提取规则；空规则列表会停用规则
cli-dialogue-rules-help = 替换与 Builtin 配合使用的 MV 对话姓名投影
cli-profile-help = 翻译 Profile ID；省略时复用上次成功 Profile
cli-terms-help = 替换项目术语资源
cli-placeholders-help = 替换项目 Placeholder 资源
cli-project-lua-script-help = 要对项目数据库运行的 Lua 脚本
cli-project-lua-arguments-help = 在 -- 后传给 Lua arg[1..] 的 UTF-8 参数
cli-manual-file-help = 人工译文 TOML 文件
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
plan-source-explicit = 显式输入
plan-source-project-state = 项目状态
plan-source-product-default = 产品行为
notice-init-reuse-path = 未提供来源路径，已沿用上次成功路径：{ $path }。
notice-extract-reuse-owners = 未提供提取范围，已沿用上次成功方案：{ $owners }。
notice-translate-reuse-profile = 未提供 Profile，已沿用上次成功 Profile：{ $profile }。
notice-no-model-request = 全部翻译单元均为最新状态，本次无需请求模型。
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
progress-no-work = 无需处理
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
result-translate-completed = 翻译运行结束：{ $project }（Profile：{ $profile }）
result-translate-status = 状态：{ $status }
result-translate-status-value = { $status ->
    [no_work] 无需处理
    [complete] 完整
    [incomplete] 未完整
   *[other] __ATT_FALLBACK__
}
result-translate-summary = 翻译：计划 { $total } 个任务，已开始 { $started }，未开始 { $not_started }；完整 { $complete }，部分 { $partial }，不可用 { $unavailable }，失败 { $failed }，取消 { $cancelled }；写入 { $written } 处，剩余 { $remaining } 处，其中 Rejected { $rejected } 处
result-translate-convergence = 状态收敛：保留 { $retained }，失效 { $invalidated }，不适用 { $not_applicable }，复用 { $reused }
result-write-back-completed = 写回完成：{ $project }
result-project-lua-completed = 项目 Lua 执行完成：{ $project }
result-output-directory = 输出目录：{ $path }
result-write-back-summary = 写回：应用译文 { $translated } 个单元，保留原文 { $original } 个单元
result-generic-extract-unchanged = Generic 输入未变化：{ $files } 个文件，{ $groups } 个组，{ $units } 个单元
result-generic-extract-updated = Generic 输入已更新：{ $files } 个文件，{ $groups } 个组，{ $units } 个单元；保留 { $preserved } 条译文，清除 { $cleared } 条
result-generic-translate-summary = Generic 翻译：计划 { $total } 个任务，已开始 { $started }，未开始 { $not_started }；完整 { $complete }，部分 { $partial }，不可用 { $unavailable }，失败 { $failed }，取消 { $cancelled }；计划 Unit { $planned_units }，剩余 Unit { $remaining_units }，其中 Rejected Unit { $rejected_units }，清除 { $cleared }，复用 { $reused }，接受 { $accepted }，写入 { $written }，冲突 { $conflicted }，响应问题 { $problems }
result-generic-write-back-summary = Generic 写回：应用译文 { $translated } 个单元，保留原文 { $original } 个单元
result-run-log = 运行记录：{ $path }
translate-incomplete-object = 项目 { $project } 的本次 Translate
translate-incomplete-rpg-maker-reason = 部分任务 { $partial }，不可用任务 { $unavailable }，未开始任务 { $not_started }，协议问题 { $protocol }，请求耗尽 { $exhausted }；请求准入{
    $admission ->
        [stopped] 已停止
       *[open] 未停止
    }；剩余决策 { $remaining_decisions }，剩余位置 { $remaining_locations }，其中 Rejected { $rejected_locations } 处
translate-incomplete-generic-reason = 部分任务 { $partial }，不可用任务 { $unavailable }，未开始任务 { $not_started }，请求耗尽 { $exhausted }；请求准入{
    $admission ->
        [stopped] 已停止
       *[open] 未停止
    }；剩余 Unit { $remaining_units }，其中 Rejected Unit { $rejected_units }，写入冲突 { $conflicted }，响应问题 { $problems }
translate-incomplete-help = 查看本次运行记录中的具体任务诊断，修正可重复的问题后再次运行 Translate；少量剩余内容可使用 Manual
translate-incomplete-rejected-help = 查看本次运行记录中的具体任务诊断；Rejected 内容可用 --retry-rejected 再次翻译，或用 manual export --selection rejected 导出后通过 Manual 处理
result-cancelled = 命令已在安全收尾后取消。
result-plan-saved = 已保存本次成功运行方案。
log-run-started = 命令 { $command } 已开始。
log-run-succeeded = 命令 { $command } 已成功完成。
log-run-failed = 命令 { $command } 失败。
log-run-outcome-unknown = 命令 { $command } 结束，但最终结果未知；请按错误中的恢复位置处理。
log-run-cancelled = 命令 { $command } 已取消。
log-performance-counters = 性能计数：SQLite 事务控制尝试 { $sqlite_control_attempted_total } 次；完整候选树校验开始 { $candidate_validation_started } 次，完成 { $candidate_validation_completed } 次。
log-lua-print = Lua：{ $message }
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
log-task-outcome-value = { $outcome ->
    [complete] 完成
    [partial] 部分完成
    [unavailable] 暂不可用
    [failed] 失败
    [not_committed_after_earlier_failure] 因前序失败未提交
    [cancelled] 已取消
   *[other] 结果无法识别
}
diagnostic-object = 对象：{ $subject }
diagnostic-error-heading = 错误：
diagnostic-warning-heading = 警告：
diagnostic-explanation = 原因：{ $reason }
diagnostic-impact = 影响：{ $impact }
diagnostic-resolution = 处理办法：{ $action }
diagnostic-related = { $relation ->
    [cleanup] 同时，清理失败：
    [rollback] 同时，回滚失败：
    [discard] 同时，丢弃候选失败：
    [finalization] 同时，收尾失败：
    [shutdown] 同时，关闭失败：
    [observability] 同时，结果呈现或记录失败：
   *[other] 同时，相关操作失败：
}
diagnostic-impact-value = { $effect ->
    [unchanged] 业务状态没有修改
    [progress_preserved] 此前确认的进度仍然保留；指出的内容没有完成
    [applied] 相关业务结果已经生效
    [applied_run_plan_not_saved] 业务结果已经生效，但本次运行方案没有保存
    [applied_finalization_failed] 业务结果已经生效，但必要收尾没有完成
    [recovery_required] 结果已经明确，但必须先处理指出的恢复现场
    [outcome_unknown] 无法确认本次操作是否生效；按处理办法恢复前不要重试或删除现场
   *[other] __ATT_FALLBACK__
}
diagnostic-resolution-value = { $code ->
    [fix_configuration] 修正指出的配置字段后重试
    [fix_input] 修正指出的输入后重试
    [fix_placeholder_rules] 修正指出的 Placeholder 规则后重试
    [review_disabled_rules] 如果这是预期结果，无需处理；否则在指出的文件中添加有效规则并重新运行 Extract
    [check_path_and_permissions] 检查路径、文件系统状态和权限
    [check_project_state] 检查并修正项目状态后重试
    [resolve_contention] 等待冲突操作结束后重试
    [check_model_service] 检查模型服务响应和账户配额
    [preserve_recovery_artifacts] 不要删除列出的恢复产物；先恢复输出，再重试
    [retry] 重试该操作
    [report_bug] 报告此 ATT 缺陷，并说明当时执行的操作
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] 缺少必填值
    [generic_extract_required] 当前 JSONL 输入与最近一次 Extract 不一致；请重新运行 att generic extract
    [conflicting_values] 提供的值互相冲突
    [invalid_syntax] 值的语法无效
    [invalid_encoding] 文本编码无效
    [invalid_value] 值不符合要求的契约
    [empty_text_capture] text 捕获为空
    [rules_owner_disabled] 选择的 Rules 文件使用 rule = []；Rules 已停用，并已删除其提取资产
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
    [stdout_write_failed] 无法写入标准输出
    [stderr_write_failed] 无法写入标准错误
    [stdout_flush_failed] 无法刷新标准输出
    [stderr_flush_failed] 无法刷新标准错误
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
    [not_regular_file] 现有目标不是普通文件
    [wrong_publisher_instance] 发布令牌属于另一个发布器实例
    [journal_corrupt] 发布恢复日志损坏或不完整
    [unexpected_artifact] 意外的文件系统产物阻塞了操作
    [interactive_session_already_open] 已有 SQLite 交互会话处于活动状态
    [backup_incomplete] SQLite 备份没有完成
    [request_serialization_failed] 无法序列化模型请求
    [response_parsing_failed] 模型响应不是有效 JSON
    [invalid_response_contract] 模型响应不符合所需响应契约
    [model_stream_incomplete] 模型流在明确终态前结束
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
diagnostic-http-status = HTTP 状态 { $status }
diagnostic-retry-after = Retry-After：{ $seconds } 秒
diagnostic-provider-code = 服务方 code：{ $code }
diagnostic-provider-type = 服务方 type：{ $kind }
diagnostic-provider-message = 服务方消息：{ $message }
diagnostic-json-position = 第 { $line } 行，第 { $column } 列
diagnostic-placeholder-rule-file = { $path } 中的 Placeholder 规则 { $number }
diagnostic-placeholder-rule-project = 当前项目的 Placeholder 规则 { $number }
manual-exported = 已导出 { $entries } 条：{ $path }
manual-checked = 有效 { $valid }，未填写 { $unfilled }，错误 { $errors }
manual-applied = 已应用 { $applied }，未填写 { $unfilled }，错误 { $errors }
manual-value = { $code ->
    [invalid_source_line] source 第 { $line } 项包含换行或 NUL
    [invalid_translation_line] translation 第 { $line } 项包含换行或 NUL
    [fixed_length] fixed 译文需要 { $expected } 项，当前为 { $actual } 项
    [fixed_blank_slot] fixed 译文第 { $line } 项必须保留空槽
    [rerun_export] 重新运行 manual export
    [rerun_export_without_controls] 重新运行 manual export，不要把换行或 NUL 写进数组项
    [rerun_export_then_fill] 重新运行 manual export 后再填写译文
    [resolve_temporary_then_rerun_export] 处理显示的固定临时路径；如有遗留对象，将其移除，然后重新运行 manual export
    [resolve_published_backup_cleanup] 两份导出已经生效；确认输出后删除显示的固定 backup 文件
    [keep_exported_type] 保留 manual export 生成的 type
   *[other] __ATT_FALLBACK__
}
task-record-title = 翻译任务
task-record-final-result-heading = 最终结果
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
task-record-requested = 要求译文：{ $requested } 项
task-record-accepted-written = 已接受：{ $accepted } 项（ID：{ $ids }），写入 { $written } 个实际位置
task-record-accepted-outcome-unknown = 已验收：{ $accepted } 项（ID：{ $ids }）；数据库提交终态无法确认
task-record-unaccepted = 未接受：{ $unaccepted } 项（ID：{ $ids }）
task-record-task-diagnostic = 任务诊断
