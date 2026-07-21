# ATT 汉化教程

ATT 当前支持 RPG Maker 的 MV 与 MZ 游戏。面对一个新游戏时，先阅读
[RPG Maker 调查与决策指南](docs/rpg-maker/README.md)：从游戏实际运行时如何消费文本
出发，确认初始化参数、文本载体及适合的提取方式。

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

## 3. 准备术语与翻译配置

从角色、职业、技能、物品、地点和系统用语等结构化材料中提炼稳定术语，并根据当前
语言对、模型服务和机器资源编写配置。术语表用于约束概念口径，不应代替字段翻译或
提取规则。

- [术语文件现行规格与制作指南](docs/rpg-maker/terminology.md)
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
