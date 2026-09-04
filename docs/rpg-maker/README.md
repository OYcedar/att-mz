# RPG Maker MV/MZ 项目

ATT 的 `mv` 与 `mz` 命令域处理 RPG Maker MV/MZ 原生 JSON 数据和事件。两者共享同一套
业务能力，项目身份、来源和工作区则各自独立保存。

支持范围是 MV 与 MZ 两代引擎，XP、VX、VX Ace 不在其中。只有 Builtin 与 Rules 都无法
完整表达的来源，才由外部操作者整理成 [Generic JSONL](../generic/jsonl.md)，再交给独立
Generic 项目处理。

## 调查游戏

建立项目之前，从游戏运行时实际消费的位置确认：

- 游戏根位置，以及根下正在使用的 MV `www/data/`、`www/js/` 或 MZ `data/`、`js/`；
- MV/MZ 版本、补丁和 MOD 实际覆盖顺序；
- 数据库、Map、CommonEvents、Troops、插件参数和自定义文件中的玩家可见文本；
- 每段文本的完整值、语境、自然顺序和写回位置；
- 哪些代码、资源名、标识符和控制符应当原样保留。

判断可见性的依据是运行时实际读取的内容，扩展名和目录名称只是线索。

## 选择提取能力

先用 [Extract 精确覆盖](extraction.md)与 [Rules 完整规格](rules.md)核对每类来源，按下表选择
能够完整提取并可逆写回的能力。Builtin 未覆盖的内容继续检查 Rules；插件位置、复杂度和数量
本身不决定是否使用 Generic。

| 内容 | 能力 |
|---|---|
| ATT 覆盖矩阵内的标准数据库、事件和系统文本 | Builtin |
| 已知数据文件、确定字段路径、启用插件参数或指定事件参数 | Extract Rules |
| MV 第一条消息行中由明确语法包裹的说话人 | MV dialogue rules |
| 已经提取但不可让模型改写的控制符或协议片段 | Placeholder |
| Builtin 与 Rules 无法形成确定、完整、可逆读写的内容 | 外部转换加独立 Generic 项目 |

Builtin 和 Rules 可以在同一项目中搭配使用，只要两者声明的物理修改互不竞争。一个
游戏同时使用 RPG Maker 与 Generic 项目时，任务清单必须记录每部分内容的唯一所有者。

## 流程

- [Init](init.md)：确认项目和冻结来源；
- [Extract](extraction.md)：执行 Builtin 与 Rules；
- [Rules](rules.md)：制作 MV 姓名投影、Extract Rules 与 RPG Maker Placeholder；
- [Translate](translation.md)：准备语境、全局去重、模型验收和 Current；
- [Manual](../manual/README.md)：导出、检查并应用仍需人工处理的 TOML 条目；
- [WriteBack](write-back.md)：从冻结来源构建并发布候选。

普通人工补译使用 Manual TOML。[Lua](../lua/README.md) 是独立的项目数据库入口，用于批量
读取上下文、复杂筛选、批量变换、诊断或特殊数据库修改；它不接触游戏文件。

完整任务顺序见[翻译项目指南](../guides/translation-project.md)，失败或不完整结果见
[诊断与恢复指南](../guides/diagnosis-and-recovery.md)，遗漏、补译、输出和实际加载见
[翻译验收指南](../guides/acceptance.md)。
