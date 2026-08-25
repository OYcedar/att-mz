# ATT 运行时导航

按观察到的问题选择规格；命令参数与状态含义不在本页重复定义。

| 当前问题 | 必读规格 |
| --- | --- |
| 命令语法、保存选择、取消、输出或退出码 | [CLI](cli.md) |
| 固定配置、语言、Profile、Client 或参数 | [配置](configuration.md) |
| HTTP、模型请求协议、连接、超时、代理、限速、重试或敏感信息 | [OpenAI-compatible HTTP](openai-compatible.md) |
| 项目数据库、Unit 状态、事务、锁、schema 或提交结果未知 | [SQLite](sqlite.md) |
| RunId、结构化诊断、任务事件、日志降级或呈现失败 | [项目日志](project-log.md) |
| candidate、stage、backup、journal、目录交换或发布终态 | [目录发布](directory-publishing.md) |
| `att.exe` 同目录必须包含什么、资源是否一致 | [发行物](distribution.md) |

出现 `failed`、`cancelled`、`recovery_required`、`outcome_unknown`、Partial、Unavailable 或
业务结果与退出码看似矛盾时，同时读取
[诊断与恢复指南](../guides/diagnosis-and-recovery.md)。

各值的责任边界是固定的：数据库拥有项目和译文状态，目录 journal 拥有发布恢复事实，
项目日志与模型任务记录只提供诊断证据。任一记录都不能替代另一责任方的权威状态。
