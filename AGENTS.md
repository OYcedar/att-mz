# AGENTS.md

## 1. 项目定位与版本边界

ATT 是聚合多个游戏引擎（`mv`、`mz`、`generic`）翻译能力的 CLI；Formic（`crates/formic/`）是同一 workspace 下的独立多轮任务执行程序，随 ATT 共享工具链、`Cargo.lock` 和版本号并一同发布。

- **单一权威来源**：外部契约与状态规则以 `docs/` 现行规格为准；指南（`docs/guides/`、`maintenance/`）与 Skill（`skills/`）只组织操作流程和验证方法，不重复定义规格。发现知识缺口时先更新对应规格，再由其他入口链接引用。
- **单一当前版本**：业务模型、输入输出协议、SQLite schema 和配置只实现当前版本，不在工作树保留历史兼容解析、迁移或双读双写；`archive/` 仅保存与当前版本无关的原始参考材料。

## 2. 架构与工程约定

- **在规则所有者处解决根因**：统一入口负责跨引擎进程级职责与路由，各引擎拥有自己的命令域、状态和错误语义，语义与生命周期一致的能力提炼为唯一共享实现。修改时在拥有该状态的模块直接解决根因，并同步更新全部受影响调用方、测试、配置、规格和发行内容。
- **当前消费者原则**：CLI、配置、外部文件、协议、日志、诊断、任务记录及内部字段、状态和抽象均须存在当前产品消费者；可稳定推导或无人消费的内容不重复输入、保存或输出。
- **配置边界**：命令行参数只表达单次运行意图；`config.toml` 只收纳部署环境、外部服务或操作者可长期选择的策略，内部并发窗口、批次和调度参数由代码负责。
- **跨引擎规划不变量**：Extract 建立稳定的 `Semantic Scope → Group → Unit` 层次，共享 Planner 按完整原文稳定装箱，再由引擎适配器处理去重、临时 ID、候选验收和提交（详见 [TaskBlock 规划规格](docs/translation/task-planning.md)）。
- **自然标识**：面向人的 CLI、配置、文件名、日志、错误、临时文件和恢复目录只使用源文件名、自然编号（如 `run-000001`）、字段名或简短业务名，不展示 hash、UUID、数据库随机主键或内部编码位置。
- **保护使用者活动配置**：发行目录中已存在的 `config.toml` 与 `tools/formic/config.toml` 属于使用者状态，资源同步或更新不得覆盖或删除；缺失时才从 `config.example.toml` 首次创建。
- **活动窗口与并发确定性**：活动窗口只限制同时在途的工作，窗口满时等待，不限制项目总文件数或总条目数。并发只改变耗时，不改变自然顺序、分组、提交顺序或持久化一致性。
- **证据与恢复权威分离**：项目日志、指标和任务记录默认只作为事后诊断证据，不参与数据库提交或目录发布恢复判断（恢复分别以 SQLite 和目录发布 journal 为准，详见 [项目日志规格](docs/runtime/project-log.md)）。
- **测试边界**：生产代码和自动化测试只验证产品可观察行为与数据契约，不得读取、解析或断言 Markdown 文档、Skill 或 `dist/` 发行副本。

## 3. 条件性必读材料

涉及下列主题时，在设计或修改前读取对应现行规格（同一行列出多个命令时，只读取本次涉及的规格）：

| 主题 | 必读材料 |
| --- | --- |
| Extract 文本层次、TaskBlock 或临时 ID | [TaskBlock 规划](docs/translation/task-planning.md) |
| 维护共享 Planner 或新增引擎的规划实现 | [TaskBlock 规划](docs/translation/task-planning.md)、[规划验证指南](maintenance/task-planning-validation.md) |
| Prompt、user message 或模型响应 | [Prompt 与模型协议](docs/translation/prompts.md) |
| 译文候选验收、强不变量、Review 或 Rejected | [译文候选验收规格](docs/translation/candidate-validation.md) |
| 模型任务记录 | [模型任务记录规格](docs/translation/task-records.md) |
| 语言判断或语言验收 | [语言规格](docs/translation/language.md) |
| Placeholder | [Placeholder 规格](docs/translation/placeholders.md) |
| 术语 | [术语规格](docs/translation/terminology.md) |
| WriteBack 排版规则、自动断行或续行补空白 | [WriteBack 排版规则规格](docs/translation/write-back-layout-rules.md) |
| Manual TOML 导出、检查或应用 | [Manual TOML 规格](docs/manual/README.md) |
| 项目数据库 Lua（高级 API 与 `ctx.db`） | [Lua 规格](docs/lua/README.md) |
| CLI 语法、输出或退出码 | [CLI 规格](docs/runtime/cli.md) |
| 配置、配置校验或固定配置目录 | [配置规格](docs/runtime/configuration.md) |
| HTTP、模型请求协议、超时、重试、代理、限速或敏感信息 | [OpenAI-compatible HTTP 规格](docs/runtime/openai-compatible.md) |
| 项目日志、诊断、RunId 或呈现失败 | [项目日志规格](docs/runtime/project-log.md) |
| SQLite schema、项目状态、事务或数据库诊断 | [SQLite 规格](docs/runtime/sqlite.md) |
| 候选目录、原子发布、恢复现场或发布终态 | [目录发布规格](docs/runtime/directory-publishing.md) |
| 性能目标、核心路径、并发或资源机制 | [性能工程要求](maintenance/performance.md) |
| 实际测量、性能候选取舍或参数实验 | [性能工程要求](maintenance/performance.md)、[性能验证指南](maintenance/performance-validation.md) |
| 发行资源或 GitHub Release | [发行物规格](docs/runtime/distribution.md)、[公开发行指南](maintenance/releasing.md) |
| Formic CLI、配置、模型与工具执行、作业状态或输出 | [Formic 维护入口](crates/formic/README.md)、[Formic 文档索引](tools/formic/docs/README.md) |
| RPG Maker Init、Extract、Rules、Translate 或 WriteBack | 对应的 [Init](docs/rpg-maker/init.md)、[Extract](docs/rpg-maker/extraction.md)、[Rules](docs/rpg-maker/rules.md)、[Translate](docs/rpg-maker/translation.md) 或 [WriteBack](docs/rpg-maker/write-back.md) 规格 |
| Generic JSONL、Init、Extract、Translate、WriteBack 或外部适配器验收 | 对应的 [JSONL](docs/generic/jsonl.md)、[Init](docs/generic/init.md)、[Extract](docs/generic/extraction.md)、[Translate](docs/generic/translation.md)、[WriteBack](docs/generic/write-back.md) 或 [外部适配器验收](docs/generic/adapter-verification.md) 规格 |

## 4. 验证与同步

- **Rust 代码改动**：提交前对受影响包或 target 执行 `cargo fmt --check`、`cargo check`、`cargo clippy` 和按行为影响确定的测试，处理当前修改范围内的 warning。配置变更须接通生产入口并覆盖默认值、合法显式值与无效值。
- **发行资源同步**：修改 `README.md`、`config.example.toml`、`docs/`、`prompts/`、`skills/`、`licenses/` 或 `tools/formic/` 用户资料后，运行 `scripts/sync-dist-resources.ps1` 同步 `dist/`。
- **翻译任务经验沉淀**：普通翻译任务只交付译本、静态 QA 和人工实机检查项；仅在任务发起者明确要求维护经验时才更新 `skills/translate-with-att/references/`（只记录可复用的客观结构与操作方法，不记录具体游戏名、本机路径或单次任务过程）。
