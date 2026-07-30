# RPG Maker MV/MZ 项目

ATT 的 `mv` 与 `mz` 命令域处理 RPG Maker MV/MZ 原生 JSON 数据和事件。它们共享业务能力，
但项目身份、来源和工作区分别保存。

不支持 XP、VX 或 VX Ace。插件脚本、自定义二进制或 Rules 无法完整表达的格式不应强塞进
RPG Maker 提取；由外部操作者整理成 [Generic JSONL](../generic/jsonl.md)，使用独立的
Generic 项目处理。

## 调查游戏

建立项目之前，从游戏运行时实际消费的位置确认：

- `www/data` 或 `data` 哪一个是当前游戏根；
- MV/MZ 版本、补丁和 MOD 实际覆盖顺序；
- 数据库、Map、CommonEvents、Troops、插件参数和自定义文件中的玩家可见文本；
- 每段文本的完整值、语境、自然顺序和写回位置；
- 不应翻译的代码、资源名、标识符和控制符。

不要只按扩展名或目录名称猜测可见性。

## 选择提取能力

| 内容 | 能力 |
|---|---|
| ATT 覆盖矩阵内的标准数据库、事件和系统文本 | Builtin |
| 已知文件、字段、插件参数或事件参数路径 | Extract Rules |
| MV 第一条消息行中由明确语法包裹的说话人 | MV dialogue rules |
| 已经提取但不可让模型改写的控制符或协议片段 | Placeholder |
| 上述能力无法完整表达的内容 | 外部转换加独立 Generic 项目 |

Builtin 和 Rules 可以在同一项目中使用，但不能声明相互竞争的物理修改。一个游戏同时使用
RPG Maker 与 Generic 项目时，任务清单必须记录每部分内容的唯一所有者。

## 流程

- [Init](init.md)：确认项目和冻结来源；
- [Extract](extraction.md)：执行 Builtin 与 Rules；
- [Rules](rules.md)：制作 MV 姓名投影、Extract Rules 与 RPG Maker Placeholder；
- [Translate](translation.md)：准备语境、全局去重、模型验收和 Current；
- [WriteBack](write-back.md)：从冻结来源构建并发布候选。

独立 [Lua](../lua/README.md) 只操作项目数据库，不参与上述阶段，也不能读取或改写游戏文件。
