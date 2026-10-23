# RPG Maker 命令名称的真实消费者

活动插件修改标题、菜单、设置或战斗指令列表，或实玩发现命令漏翻、指令消失时，按本页排查。

## 从命令列表回溯显示值

1. 按插件加载顺序查找命令列表构建代码（`makeCommandList`、`addGeneralOptions`、`addVolumeOptions`、`addOriginalCommands`、`addCustomOptions` 及直接操作 `_list` 的 `addCommand`、`push`、`splice`）。
2. 将 `addCommand(name, symbol)` 中的 `name` 变量沿赋值链回溯到 `PluginManager.parameters`、包装 getter、默认值或嵌套结构。
3. 只提取最终进入显示调用的 `name` 字段；保持技术 `symbol`、handler、开关名和布尔配置不变。若插件把显示名称同时用作查找键，需在外部适配中保持查找映射一致。

## 备注协议中的类型与显示名称

插件常把数据库备注（note）按分隔符拆为多个槽位（例如 `<command:kind,order,caption,icon>`）：

- 核对插件解析器取出的各槽位用途：进入 `symbol` 分派或 ID 查找的是协议值（即使写了日文/英文单词也不能翻译），进入 `addCommand` 显示的才是文案。
- 按 [Extract Rules](../../../docs/rpg-maker/rules.md#4-extract-rules) 仅将确认的显示槽捕获为 `text`，其余部分保留为 Literal；若备注结构超出 Rules 表达能力，改用 Generic 外部映射处理。
- 若译后出现菜单指令消失，优先检查是否误译了备注中的协议类型、symbol 或过滤条件，恢复原版协议值并修正提取规则。

## 图片中的命令名称

若数据库与插件参数均已汉化而菜单按钮仍显示原文，检查窗口是否使用图片绘制按钮（含普通态、选中态、禁用态切片或加密图片资源）。图片按钮交由图像翻译流程处理，保持原尺寸、透明通道与文件名不变。
