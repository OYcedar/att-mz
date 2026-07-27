app-about = 使用可复用项目状态翻译 RPG Maker 游戏
cli-config-help = 本次运行使用的严格 TOML 配置文件
cli-ui-language-help = Help、诊断、进度、结果和项目日志使用的语言：ar、zh-Hans、zh-Hant、en、fr、ru、es、ja、ko 或 vi
cli-progress-help = 实时进度模式：auto、plain 或 off
cli-mz-about = 翻译 RPG Maker MZ 游戏
cli-mv-about = 翻译 RPG Maker MV 游戏
cli-init-about = 初始化或更新一个命名游戏项目
cli-extract-about = 使用显式或已保存的 owner 方案提取原文
cli-translate-about = 使用显式或已保存的 Profile 翻译已提取原文
cli-write-back-about = 将已验收译文写回游戏
cli-project-lua-about = 在项目上下文中一次性运行可信 Lua 程序
cli-project-name-help = 稳定项目名称
cli-init-path-help = RPG Maker 游戏根目录；已有项目可复用上次成功路径
cli-source-language-help = 原文语言 ID
cli-target-language-help = 译文目标语言 ID
cli-dialogue-width-help = 对话正文每行允许的最大全角字符数
cli-scrolling-width-help = 滚动文本每行允许的最大全角字符数
cli-help-width-help = 帮助或说明框每行允许的最大全角字符数
cli-builtin-help = 使用 ATT 内置的 RPG Maker 文本位置
cli-rules-help = 用该 TOML 定义替换 Rules owner；空规则列表会停用它
cli-dialogue-rules-help = 替换与 Builtin 配合使用的 MV 对话姓名投影
cli-lua-help = 替换当前阶段的 Lua 程序；零字节文件会清除它
cli-profile-help = 翻译 Profile ID；省略时复用上次成功 Profile
cli-terms-help = 替换项目术语资源
cli-placeholders-help = 替换项目 Placeholder 资源
cli-project-lua-profile-help = Standard 人工验收使用的 Profile；省略时在打开 Standard 能力时复用上次成功的 Translate Profile
cli-project-lua-script-help = 本次一次性运行的可信 Lua 程序
cli-project-lua-arguments-help = 在 -- 后传给 Lua arg[1..] 的 UTF-8 参数
cli-usage-heading = 用法：
cli-commands-heading = 命令：
cli-options-heading = 选项：
cli-arguments-heading = 参数：
cli-options-metavar = 选项
cli-command-metavar = 命令
cli-print-help = 显示帮助
cli-print-version = 显示版本
cli-missing-config = 缺少必需的配置路径 --config <FILE>。
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
log-label-phase-check-project = 检查项目
log-label-phase-scan-source = 扫描来源
log-label-phase-prepare-candidate = 准备候选目录
log-label-phase-update-database = 更新数据库
log-label-phase-publish = 发布结果
log-label-phase-builtin = 内置提取
log-label-phase-rules = 规则提取
log-label-phase-lua = Lua 处理
log-label-phase-planning = 规划任务
log-label-phase-confirmed-tasks = 确认任务
log-label-phase-no-work = 无需处理
log-label-phase-read-assets = 读取资产
log-label-phase-plan-standard = 规划标准写回
log-label-phase-rewrite-documents = 改写文档
log-label-phase-validate-candidate = 验证候选目录
log-label-task-complete = 完整
log-label-task-partial = 部分可用
log-label-task-unavailable = 不可用
log-label-task-failed = 失败
error-state-applied-finalization = 结果已经生效，但收尾失败。重试前请先检查项目状态。
error-no-executable-extract-owner = 清除后没有可执行的 Extract owner，因此未保存方案。
error-plan-save-failed-applied = 命令结果已生效，但新运行方案未保存。下次请显式传入预期选项。
error-plan-save-outcome-unknown = 命令结果已生效，但无法确认运行方案提交结果。下次请显式传入预期选项。
plan-source-explicit = 显式输入
plan-source-project-state = 项目状态
plan-source-product-default = 产品行为
notice-init-reuse-path = 未提供来源路径，已沿用上次成功路径：{ $path }。
notice-extract-reuse-owners = 未提供提取范围，已沿用上次成功方案：{ $owners }。
notice-translate-reuse-profile = 未提供 Profile，已沿用上次成功 Profile：{ $profile }。
notice-translate-reuse-lua = 未提供 Lua 选项，已沿用上次成功的 Translate Lua 选择。
notice-write-back-reuse-lua = 未提供 Lua 选项，已沿用上次成功的 WriteBack Lua 程序。
notice-write-back-standard-only = 尚未配置 WriteBack Lua 程序，本次仅执行 Standard。
notice-owner-disabled = 已停用 owner { $owner }，并将其移出后续自动方案。
notice-lua-cleared = 已清除 { $phase } Lua 程序，本轮不会执行。
notice-no-model-request = 全部标准翻译单元均为最新状态，Standard 本次未请求模型。
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
progress-extract-lua = 正在运行 Extract Lua 程序
progress-extract-commit = 正在提交提取资产
progress-translate-planning = 正在规划翻译任务
progress-translate-confirmed = 已确认翻译任务
progress-translate-no-work = 无需调用模型
progress-project-lua = 正在运行项目 Lua 程序
progress-write-back-read-assets = 正在读取已验收资产
progress-write-back-planning = 正在规划文档改写
progress-write-back-documents = 已改写文档
progress-write-back-lua = 正在运行 WriteBack Lua 程序
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
result-translate-standard = 标准翻译：任务 { $total }，完整 { $complete }，部分 { $partial }，不可用 { $unavailable }；写入 { $written } 处，剩余 { $remaining } 处
result-translate-convergence = 状态收敛：保留 { $retained }，失效 { $invalidated }，不适用 { $not_applicable }，复用 { $reused }
result-write-back-completed = 写回完成：{ $project }
result-project-lua-completed = 项目 Lua 执行完成：{ $project }
result-output-directory = 输出目录：{ $path }
result-write-back-standard = 标准写回：应用译文 { $translated } 个单元，保留原文 { $original } 个单元；自动换行 { $auto_wrapped } 段，新增换行 { $breaks } 处；续行全角缩进 { $indents } 处；需人工换行 { $manual } 段
result-lua-executed = Lua：已执行
result-lua-not-executed = Lua：未执行
result-cancelled = 命令已在安全收尾后取消。
result-plan-saved = 已保存本次成功运行方案。
result-translate-plan-sources = 已保存本次成功运行方案。Profile 来源：{ $profile_source }；Lua 来源：{ $lua_source }。
log-run-started = 命令 { $command } 已开始。
log-run-succeeded = 命令 { $command } 已成功完成。
log-run-failed = 命令 { $command } 失败。
log-run-outcome-unknown = 命令 { $command } 结束，但最终结果未知；请按错误中的恢复位置处理。
log-run-cancelled = 命令 { $command } 已取消。
log-performance-counters = 性能计数：SQLite 事务控制尝试 { $sqlite_control_attempted_total } 次；完整候选树校验开始 { $candidate_validation_started } 次，完成 { $candidate_validation_completed } 次。
log-plan-resolved = 命令 { $command } 的方案来自{ $source }。
log-phase-started = 阶段开始：{ $phase }。
log-phase-finished = 阶段完成：{ $phase }。
log-retry-summary = 共执行 { $count } 次重试。
log-no-work = 无需执行工作：{ $reason }。
log-no-work-translation-up-to-date = 译文已经与当前来源和配置档一致
log-partial-result = 有 { $count } 个部分结果需要关注。
log-translation-task-started = 翻译任务 { $index }/{ $total } 已开始。
log-translation-task-finished = 翻译任务 { $index } 已结束，结果为 { $outcome }。
log-translation-task-diagnostic = 翻译任务 { $index } 在尝试 { $attempts } 次后报告诊断：{ $diagnostic }
diagnostic-title = 错误 [{ $code }]
diagnostic-stage = 阶段：{ $stage }
diagnostic-subject = 位置：{ $subject }
diagnostic-subject-value = { $kind ->
    [command] 命令 { $value }
    [field] 字段 { $value }
    [project] 项目 { $value }
    [profile] 配置档 { $value }
    [component] 组件 { $value }
   *[other] { $value }
}
diagnostic-reason = 原因：{ $reason }
diagnostic-impact = 影响：{ $impact }
diagnostic-action = 处理办法：{ $action }
diagnostic-recovery = 恢复位置：{ $recovery }
diagnostic-recovery-value = { $kind ->
    [component] 组件 { $value }
    [transaction] 事务 { $value }
   *[other] { $value }
}
diagnostic-related = 相关错误 { $index }：
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
   *[other] __ATT_FALLBACK__
}
diagnostic-impact-value = { $code ->
    [unchanged] 状态未改变
    [valid_progress_preserved] 已保留有效进度
    [result_applied_but_run_plan_not_saved] 结果已生效，但运行方案未保存
    [state_applied_but_finalization_failed] 状态已生效，但收尾未完成
    [recovery_required] 必须先恢复，才能信任当前状态
    [outcome_unknown] 最终状态未知
   *[other] __ATT_FALLBACK__
}
diagnostic-action-value = { $code ->
    [fix_configuration] 修正指出的配置字段后重试
    [fix_input] 修正指出的输入后重试
    [check_path_and_permissions] 检查路径、文件系统状态和权限
    [check_project_state] 检查并修正项目状态后重试
    [retry_after_resolving_contention] 等待冲突操作结束后重试
    [check_model_service] 检查模型服务响应和账户配额
    [preserve_recovery_artifacts] 不要删除列出的恢复产物；先恢复输出，再重试
    [retry] 重试该操作
    [report_bug] 携带错误码和日志路径报告 ATT 缺陷
   *[other] __ATT_FALLBACK__
}
diagnostic-failure-value = { $code ->
    [missing_required_value] 缺少必填值
    [extract_plan_required] 项目没有可复用的 Extract 方案；必须提供 --builtin、--rules 或 --lua 中的至少一项
    [conflicting_values] 提供的值互相冲突
    [invalid_syntax] 值的语法无效
    [invalid_encoding] 文本编码无效
    [invalid_value] 值不符合要求的契约
    [not_found] 所需对象不存在
    [busy] 资源正被另一项操作占用
    [state_mismatch] 已保存的项目状态不满足本次操作
    [requirement_failed] 必要前置条件未满足
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
    [lua_database_open_failed] Lua Host 无法打开项目数据库会话
    [lua_context_creation_failed] Lua 运行时无法建立 VM 上下文
    [lua_compilation_failed] Lua 主程序编译失败
    [lua_execution_failed] Lua 主程序运行失败
    [lua_host_call_failed] Lua Host 能力调用失败
    [lua_finalization_failed] Lua Host 无法完成所有绑定资源的收尾
    [lua_unclosed_transaction] Lua 程序结束时事务仍未关闭；该事务已回滚
    [lua_snapshot_store_failed] 无法提交已验证的 Lua 提取快照
    [rules_definition_invalid] Rules 程序不符合 Rules 定义契约
    [rules_document_read_failed] 无法读取 Rules 程序需要的来源文档
    [rules_no_non_blank_match] Rules 条目没有产生任何非空白语义单元
    [rules_invalid_target] Rules 条目选中的值不能作为文本目标
    [rules_pattern_match_failed] 无法执行 Rules 的 PCRE2 模式
    [rules_zero_width_match] Rules 模式产生了零宽匹配
    [rules_overlapping_capture] Rules 模式产生了相互重叠的文本捕获
    [rules_missing_text_capture] 必需的命名文本捕获没有参与匹配
    [rules_invalid_capture_range] Rules 匹配或捕获范围不在有效 UTF-8 字符边界上
    [rules_duplicate_target] 两个 Rules 条目声明了同一个物理文本目标
    [rules_invalid_materialization] Rules 投影配方无法重建来源值
    [rules_snapshot_invalid] 提取出的 Rules 组无法组成有效资产快照
    [rules_snapshot_store_failed] 无法提交已验证的 Rules 提取快照
    [write_back_extraction_out_of_date] 已提取资产不再匹配当前项目来源
    [write_back_asset_snapshot_invalid] 已保存的 Standard 资产无法组成有效写回快照
    [source_document_invalid] RPG Maker 来源文档不符合所需文档格式
    [write_back_mutation_invalid] 已验证的译文修改无法应用到冻结来源位置
    [write_back_output_path_invalid] 改写文件位于允许的 RPG Maker 输出树之外
    [write_back_output_path_duplicate] 多个改写文件指向同一输出路径
    [write_back_candidate_project_mismatch] 写回候选属于另一个项目
    [write_back_candidate_invalid] 写回候选不符合所需的 data/js 目录结构
    [write_back_unexpected_lua_outcome] Lua 写回程序返回了其他 Lua 阶段的结果
    [write_back_not_published] 写回候选没有替换当前输出目录
    [write_back_published_with_residuals] 输出已发布，但部分恢复产物无法删除
    [write_back_recovery_required] 必须先恢复输出目录，才能信任其中内容
    [internal_invariant] 内部不变量被破坏；这是 ATT 缺陷
   *[other] __ATT_FALLBACK__
}
diagnostic-io-kind-value = { $code ->
    [not_found] 对象不存在
    [permission_denied] 权限不足
    [connection_refused] 连接被拒绝
    [connection_reset] 连接被重置
    [host_unreachable] 主机不可达
    [network_unreachable] 网络不可达
    [connection_aborted] 连接被中止
    [not_connected] 尚未连接
    [address_in_use] 地址已被占用
    [address_not_available] 地址不可用
    [network_down] 网络已断开
    [broken_pipe] 管道已断开
    [already_exists] 对象已存在
    [would_block] 操作会阻塞
    [not_a_directory] 对象不是目录
    [is_a_directory] 对象是目录
    [directory_not_empty] 目录不为空
    [read_only_filesystem] 文件系统为只读
    [stale_network_file_handle] 网络文件句柄已经失效
    [invalid_input] 操作输入无效
    [invalid_data] 数据无效
    [timed_out] 操作超时
    [write_zero] 写入没有取得进展
    [storage_full] 存储空间已满
    [not_seekable] 对象不支持定位
    [quota_exceeded] 存储配额已用尽
    [file_too_large] 文件超过底层系统可表示范围
    [resource_busy] 资源正忙
    [executable_file_busy] 可执行文件正被占用
    [deadlock] 操作会造成死锁
    [crosses_devices] 操作跨越文件系统设备
    [too_many_links] 文件系统链接过多
    [invalid_filename] 文件名无效
    [argument_list_too_long] 操作系统参数列表过长
    [interrupted] 操作被中断
    [unsupported] 不支持该操作
    [unexpected_eof] 文件意外结束
    [out_of_memory] 操作系统无法分配内存
    [other] 其他操作系统错误
   *[unknown] __ATT_FALLBACK__
}
diagnostic-configuration-rule-value = { $code ->
    [runtime_configuration_invalid] 运行时配置无效
    [unsupported_prompt_locale] 必须是全小写的 auto 或受支持的 BCP 47 界面语言
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
diagnostic-io-reason = 操作 { $operation }：{ $kind }
diagnostic-io-reason-with-os-code = 操作 { $operation }：{ $kind }（OS { $os_code }）
diagnostic-io-reason-with-system-message = 操作 { $operation }：{ $kind }：{ $system_message }
diagnostic-io-reason-with-os-code-and-system-message = 操作 { $operation }：{ $kind }（OS { $os_code }）：{ $system_message }
diagnostic-failure-with-detail = { $failure }：{ $detail }
diagnostic-invalid-utf8 = 第 { $valid_up_to } 字节处的 UTF-8 无效，无效长度为 { $error_len } 字节
diagnostic-incomplete-utf8 = 第 { $valid_up_to } 字节后是未完成的 UTF-8 序列
diagnostic-toml-failure-value = { $code ->
    [syntax] TOML 语法无效
    [missing_field] 缺少必填配置字段
    [unknown_field] 配置包含未知字段
    [duplicate_field] 配置字段被重复声明
    [type_mismatch] 应为{ $expected }
    [invalid_value] 配置值不符合字段契约
   *[other] __ATT_FALLBACK__
}
diagnostic-toml-expected-kind-value = { $code ->
    [string] 字符串
    [integer] 整数
    [boolean] 布尔值
    [string_or_boolean] 字符串或布尔值
    [string_array] 字符串数组
    [integer_array] 整数数组
    [string_pair_array] 字符串对数组
    [table] 表
    [table_array] 表数组
   *[other] __ATT_FALLBACK__
}
diagnostic-invalid-toml = TOML 无效（{ $resource }）：{ $failure }
diagnostic-invalid-toml-at = TOML 第 { $line } 行、第 { $column } 列无效（{ $resource }）：{ $failure }
diagnostic-http-no-details = 模型服务请求失败，但没有返回可公开的 HTTP 状态详情
diagnostic-http-status = HTTP 状态码 { $status }
diagnostic-http-retry-after = Retry-After { $seconds } 秒
diagnostic-http-provider-code = 供应商错误码 { $code }
diagnostic-http-provider-type = 供应商错误类型 { $kind }
diagnostic-http-fact-separator = ；
diagnostic-sqlite = SQLite 主错误码 { $primary_code }，扩展错误码 { $extended_code }
diagnostic-windows-status = Windows 操作 { $operation } 失败，NTSTATUS { $status }
diagnostic-resource = { $resource }：实际值 { $actual }
diagnostic-resource-with-maximum = { $resource }：实际值 { $actual }，上限 { $maximum }
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
    [json] 模型响应 JSON 无效（类别 `{ $category }`），第 { $line } 行、第 { $column } 列
    [thinking_not_allowed] 当前响应模式不接受思考输出，第 { $line } 行、第 { $column } 列
    [thinking_envelope_missing] 模型响应缺少规定的思考信封，第 { $line } 行、第 { $column } 列
    [thinking_envelope_unclosed] 模型响应的思考信封没有闭合，第 { $line } 行、第 { $column } 列
    [thinking_empty] 模型响应的思考内容为空，第 { $line } 行、第 { $column } 列
    [thinking_nested] 模型响应包含嵌套的思考信封，第 { $line } 行、第 { $column } 列
    [thinking_repeated] 模型响应包含重复的思考信封，第 { $line } 行、第 { $column } 列
    [markdown_fence_no_body] Markdown 围栏没有正文，第 { $line } 行、第 { $column } 列
    [markdown_fence_unsupported] 只接受无语言标记或 json 标记的单层 Markdown 围栏，第 { $line } 行、第 { $column } 列
    [markdown_fence_unclosed] Markdown 围栏没有闭合，第 { $line } 行、第 { $column } 列
   *[markdown_fence_invalid_closing] Markdown 围栏必须以最终独立行闭合，第 { $line } 行、第 { $column } 列
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
task-record-rejected-heading = 未接受：
task-record-rejected-item = { $id }：{ $reason }
task-record-protocol-diagnostic = 协议诊断：{ $diagnostic }
task-record-unavailable-reason = 不可用原因：{ $reason }
task-record-task-diagnostic = 任务诊断：`{ $code }`；原因 { $reason }
task-record-rejection-reason = { $code ->
    [missing] 缺少模型输出
    [duplicate] 重复模型输出
    [invalid_shape] { $detail }
    [invalid_shape_array] 译文必须是字符串数组
    [invalid_shape_item] 译文数组第 { $line } 项必须是字符串
    [line_count_mismatch] 行数不匹配（预期 { $expected }，实际 { $actual }）
    [invalid_line_text] 第 { $line } 行包含无效控制字符
    [blank_line_mismatch] 第 { $line } 行空白状态不匹配（预期{ $expected_blank ->
        [blank] 空白
       *[other] 非空白
    }）
    [blank_translation] 译文为空
    [no_natural_language_text] 译文没有自然语言文本
    [contains_byte_order_mark] 译文包含 BOM
    [placeholder_mismatch] 占位符不匹配：{ $detail }
    [unexpected_placeholder] 出现未知占位符：{ $detail }
    [placeholder_normalization_ambiguous] 占位符规范化存在歧义：{ $detail }
    [source_residual] 检测到源语言残留：{ $detail }
   *[other] { $detail }
}
task-record-protocol-detail = { $code ->
    [non_stop_finish] finish reason 不是 stop：{ $detail }
    [invalid_response] { $detail }
    [invalid_id] 模型第 { $index } 个条目的 ID 非法
    [unknown_id] 模型第 { $index } 个条目返回了未知 ID { $detail }
   *[other] { $detail }
}
task-record-unavailable-detail = { $code ->
    [model_response_unusable] 模型响应无法解析
    [all_outputs_rejected] 所有模型输出均未通过验收
    [recoverable_request_exhausted] 可恢复请求重试预算耗尽
    [retry_after_exceeds_maximum] Retry-After 超过已配置最大等待时间
   *[other] { $code }
}
task-record-duration-seconds = { $value } 秒
task-record-duration-milliseconds = { $value } 毫秒
