# Generic 项目

Generic 是 ATT 面向任意游戏或文本的 JSONL 翻译域。它只理解
[固定 JSONL 契约](jsonl.md)，不识别原游戏格式，也不负责把译文放回游戏。

适合使用 Generic 的情况包括：

- 游戏引擎没有专用 ATT 支持；
- MV/MZ Builtin 与 Rules 无法完整覆盖某些插件或脚本文本；
- 外部操作者已经能稳定提取、定位并重新写回文本；
- 一批普通文本需要使用 ATT 的语言、术语、Placeholder、全局去重与模型执行能力。

外部操作者或工具负责生成 JSONL 和消费译后 JSONL。ATT 不要求知道生成方式。

## 项目流程

1. [Init](init.md) 绑定外部 JSONL 根和语言对；
2. [Extract](extraction.md) 读取当前 JSONL 并更新项目状态；
3. [Translate](translation.md) 处理当前未译内容；
4. [WriteBack](write-back.md) 生成译后 JSONL。

可复制输入包括 [JSONL 示例](examples/sample.jsonl)、
[Generic Placeholder 示例](examples/placeholders.toml)和
[Generic 术语示例](examples/terminology.toml)。字段与行为仍以相应现行规格为准。

Generic 来源可以长期修改。添加、删除、移动或改写内容后继续使用同一个项目，但必须先
重新 Extract。Translate 和 WriteBack 不会暗中同步来源。

同一个游戏的 Generic 项目与 MV/MZ 项目完全独立。先明确每段内容由哪个项目负责，避免
重复提取或重复写回。
