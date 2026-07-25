# ATT 配置与运行时导航

ATT 的生产配置只包含真实外部选择；完整字段见
[`config.example.toml`](../../config.example.toml)，严格契约见
[生产配置](configuration.md)。

## 按命令准备配置

| 命令 | 实际消费的配置 |
|---|---|
| Init | `projects.root` |
| Extract | `projects.root`；Builtin、Rules、Lua 来自 CLI 或保存方案 |
| Translate | `projects.root`、`prompts`、`rpg_maker.record_translation_tasks`、全部 `languages`、所选 Profile 和它引用的 Client |
| WriteBack | `projects.root`；Lua 选择来自 CLI 或保存方案 |
| Lua | `projects.root`；脚本始终来自本次 CLI；只有调用 `ctx.standard.open()` 时才需要显式或已保存的 Translate Profile |

Help 与 Version 之外的命令必须显式传入 `--config FILE`。配置中的相对路径以配置文件
目录为基准；其他 CLI 路径以当前工作目录为基准。项目工作区固定是
`<projects.root>/<engine>/<project-name>`。

省略 CLI 参数表示复用项目中已经成功保存的事实，不表示从配置中猜默认业务方案：

- Init 可复用上次成功来源和 metadata；
- Extract 可复用完整 owner 集合；
- Translate 可复用 Profile、canonical 术语/Placeholder 和 Lua 阶段快照；
- WriteBack 可复用 Lua 选择。

独立 `lua` 命令不保存运行方案或程序快照。它每次读取显式脚本，可选地通过
`ctx.standard` 使用项目 canonical 术语、Placeholder 和 Standard 核心完成已有人工作品的
验收与原子提交；它不请求 LLM，也不改变以后 Translate 复用的 Profile。

## 哪些值由用户配置

用户可配置的 LLM 相关值包括显式 Standard 任务记录选择和模型供应商真实约束：

- 可选的 Standard TaskBlock 可读记录开关；
- Client 最大并发请求数；
- 可选 RPM 与 burst；
- 连接、连续读取和完整请求超时；
- 重试延迟与可接受的最大 `Retry-After`；
- 代理和额外 PEM 根证书。

Profile 只配置所用 Client 和普通任务最终 user message 的字符装箱目标。该目标调节任务
粒度，不构成内容合法性、Provider 上下文或请求容量上限。Prompt、语言策略、项目根与
业务规则继续按各自规格配置。

## 哪些值不能配置

Tokio/Rayon/file/SQLite worker、内部在途窗口、队列、批次、缓存、日志刷新与同步策略由
执行者根据运行时事实和最大真实游戏基准拥有。文件、目录、Lua、SQLite 结果以及
Claim、Unit、Group、Task 总量没有 ATT 人工上限；当前顶层配置只包含项目、Prompt、
LLM、语言和 RPG Maker 业务配置。

饱和只让上游等待并响应取消，不产生本地队列已满、准入超时或“项目过大”。项目
锁、发布锁和 SQLite busy 持续等待；LLM 本地等待不计为模型失败。

## 性能与正确性

大改动或可能影响核心路径的改动必须在 `tmp/` 中使用测试集最大、最复杂的真实游戏做
Release/MSVC 配对基准。每轮使用全新来源、项目数据库和输出，不得走 unchanged、旧译文、
旧写回或持久缓存捷径。目标是 Extract、排除模型生成/网络等待后的 Translate、WriteBack
中位耗时分别不超过 10 秒，同时保持自然顺序、指纹、全局去重、事务、Lua、验证和单次
发布等全部语义。

## 继续阅读

- [CLI 与运行方案](cli.md)
- [Chat Completions](chat-completions.md)
- [SQLite](sqlite.md)
- [目录发布](directory-publishing.md)
- [项目日志](project-log.md)
- [Standard 翻译任务记录](../rpg-maker/task-records.md)
- [RPG Maker 文档](../rpg-maker/README.md)
