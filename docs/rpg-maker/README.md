# RPG Maker MV/MZ 项目

ATT 的 `mv` 与 `mz` 命令域处理 RPG Maker MV/MZ 原生 JSON 数据和事件。两者共享同一套
业务能力，项目身份、来源和工作区则各自独立保存。

支持范围是 MV 与 MZ 两代引擎，XP、VX、VX Ace 不在其中。插件脚本、自定义二进制或
Rules 无法完整表达的格式，先由外部操作者整理成 [Generic JSONL](../generic/jsonl.md)，
再交给独立的 Generic 项目处理。

## 调查游戏

建立项目之前，从游戏运行时实际消费的位置确认：

- `www/data` 或 `data` 哪一个是当前游戏根；
- MV/MZ 版本、补丁和 MOD 实际覆盖顺序；
- 数据库、Map、CommonEvents、Troops、插件参数和自定义文件中的玩家可见文本；
- 每段文本的完整值、语境、自然顺序和写回位置；
- 哪些代码、资源名、标识符和控制符应当原样保留。

判断可见性的依据是运行时实际读取的内容，扩展名和目录名称只是线索。

## 选择提取能力

| 内容 | 能力 |
|---|---|
| ATT 覆盖矩阵内的标准数据库、事件和系统文本 | Builtin |
| 已知文件、字段、插件参数或事件参数路径 | Extract Rules |
| MV 第一条消息行中由明确语法包裹的说话人 | MV dialogue rules |
| 已经提取但不可让模型改写的控制符或协议片段 | Placeholder |
| 上述能力无法完整表达的内容 | 外部转换加独立 Generic 项目 |

Builtin 和 Rules 可以在同一项目中搭配使用，只要两者声明的物理修改互不竞争。一个
游戏同时使用 RPG Maker 与 Generic 项目时，任务清单必须记录每部分内容的唯一所有者。

## 流程

- [Init](init.md)：确认项目和冻结来源；
- [Extract](extraction.md)：执行 Builtin 与 Rules；
- [Rules](rules.md)：制作 MV 姓名投影、Extract Rules 与 RPG Maker Placeholder；
- [Translate](translation.md)：准备语境、全局去重、模型验收和 Current；
- [WriteBack](write-back.md)：从冻结来源构建并发布候选。

独立 [Lua](../lua/README.md) 的舞台是项目数据库：它不进入上述阶段，也不接触游戏
文件。
