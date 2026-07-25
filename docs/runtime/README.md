# ATT 配置与运行时导航

ATT 的生产配置只包含真实外部选择；完整字段见
[`config.example.toml`](../../config.example.toml)，严格契约见
[生产配置](configuration.md)。

## 按命令准备配置

| 命令 | 实际消费的配置 |
|---|---|
| Init | `projects.root` |
| Extract | `projects.root`；Builtin、Rules、Lua 来自 CLI 或保存方案 |
| Translate | `projects.root`、`prompts`、全部 `languages`、所选 Profile 和它引用的 Client |
| WriteBack | `projects.root`；Lua 选择来自 CLI 或保存方案 |

Help 与 Version 之外的命令必须显式传入 `--config FILE`。配置中的相对路径以配置文件
目录为基准；其他 CLI 路径以当前工作目录为基准。项目工作区固定是
`<projects.root>/<engine>/<project-name>`。

省略 CLI 参数表示复用项目中已经成功保存的事实，不表示从配置中猜默认业务方案：

- Init 可复用上次成功来源和 metadata；
- Extract 可复用完整 owner 集合；
- Translate 可复用 Profile、canonical 术语/Placeholder 和 Lua 阶段快照；
- WriteBack 可复用 Lua 选择。

## 哪些值由用户配置

用户配置的资源相关值只限模型供应商真实约束：

- Client 最大并发请求数；
- 可选 RPM 与 burst；
- 连接、连续读取和完整请求超时；
- 重试延迟与可接受的最大 `Retry-After`；
- 代理和额外 PEM 根证书。

Profile 只配置所用 Client 和单任务最终 user message 字符上限。Prompt、语言策略、项目根
与业务规则继续按各自规格配置。

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
- [RPG Maker 文档](../rpg-maker/README.md)
