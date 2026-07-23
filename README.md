# ATT 汉化教程

ATT 当前支持 RPG Maker 的 MV 与 MZ 游戏。面对一个新游戏时，先阅读
[RPG Maker 调查与决策指南](docs/rpg-maker/README.md)：从游戏实际运行时如何消费文本
出发，确认初始化参数、文本载体及适合的提取方式。

CLI 会按 `--ui-language`、`ATT_UI_LANGUAGE`、Windows 用户首选 UI 语言、英语的顺序选择
界面语言；支持 `ar`、`zh-Hans`、`zh-Hant`、`en`、`fr`、`ru`、`es`、`ja`、`ko` 与
`vi`。`--progress auto|plain|off` 控制实时进度：`auto` 只在 stderr 是终端时动态显示，
`plain` 输出适合日志采集的稀疏阶段行，`off` 关闭实时状态。最终摘要写 stdout，进度、
友好提示与错误写 stderr。

已有项目会保存 Init、Extract、Translate 与 WriteBack 各自上次成功的运行方案。省略
本次无需重新选择的参数时，ATT 会明确提示并沿用项目状态；显式参数则按对应命令的
现行规格替换方案。运行方案只在业务成功且必要收尾完成后保存，项目日志故障不会改变
业务结果或退出码。

## 1. 初始化项目

先确认游戏版本、正确的游戏根目录、源语言、目标语言，以及对话、滚动文本和帮助说明
三类宽度。不要仅凭目录名或少量文本猜测这些事实。

- [初始化现行规格](docs/rpg-maker/init.md)
- [配置编写与运行能力导航](docs/runtime/README.md)
- [运行时与 CLI 现行规格](docs/runtime/cli.md)

## 2. 提取文本

先核对 Builtin 能否覆盖玩家实际可见的标准文本，再为已确认的插件参数、事件参数或
自定义 JSON 编写 Rules。只有真实关系无法由 Builtin 或 Rules 完整、可逆地表达时，
才需要 Lua。

- [文本提取现行规格](docs/rpg-maker/extraction.md)
- [规则文件现行规格与编写指南](docs/rpg-maker/rules.md)

提取完成后，同时抽查误收与漏收；命令成功不等于所有玩家可见文本都已正确进入项目。
项目数据库中的 `standard_mutation_claim` 是由完整 recipe 派生的跨 owner 冲突摘要，
不是完整逻辑 Claim 清单；核对或修复标准资产时，应同时检查 group kind、位置、recipe、
owner 指纹和该摘要，不能只按表中行数判断提取覆盖。

## 3. 准备术语与翻译配置

从角色、职业、技能、物品、地点和系统用语等结构化材料中提炼稳定术语，并根据当前
语言对和模型服务编写配置。CPU、文件、SQLite 的并发及内部工作窗口由 ATT 根据真实
基准和运行时事实负责，不要求用户把机器资源调优伪装成业务配置。术语表用于约束概念
口径，不应代替字段翻译或提取规则。

Translate 的 `[prompts]` 必须提供 `root`、`locale` 与 `thinking_output`。`locale = "auto"`
复用本进程有效 UI locale，显式值则覆盖它；提示词语言与游戏源/目标语言彼此独立。
资源位于 `<prompts.root>/rpg_maker/<locale>/{system.md,thinking.md}`，关闭思考输出时不会读取
`thinking.md`。system 模板只以 `{{source_language}}`、`{{target_language}}` 接收项目
规范 LanguageId；资源缺失或模板无效会在首次模型请求前失败，不做任何 locale 回退。

- [术语文件现行规格与制作指南](docs/rpg-maker/terminology.md)
- [系统提示词编写指南](docs/rpg-maker/prompts.md)
- [配置编写与运行能力导航](docs/runtime/README.md)
- [生产配置现行规格](docs/runtime/configuration.md)

## 4. 翻译并检查结果

标准资产可以直接使用 Standard Translate，不需要为了调用模型而启用 Lua。检查任务的
`Complete`、`Partial` 与 `Unavailable` 结果及未解决原因，不能只看退出码。

- [翻译现行规格](docs/rpg-maker/translation.md)

## 5. 写回与游戏内验证

WriteBack 从冻结来源重新建立候选树，不会修改原游戏，也不会生成完整游戏包。检查实际
变化是否只落在预期文本位置，并验证控制符、未知字段、选择项关联、嵌套 JSON 和界面
宽度。若发现漏收、误收或错误边界，应回到提取判断修正来源，而不是直接修补候选文件。

- [写回现行规格](docs/rpg-maker/write-back.md)

需要处理声明式规则无法表达的自定义协议时，阅读
[Lua 技术参考](docs/rpg-maker/lua.md)与
[Lua Cookbook](docs/rpg-maker/lua-cookbook.md)。
