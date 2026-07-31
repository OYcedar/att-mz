# Generic 项目

Generic 是 ATT 面向任意游戏或文本的 JSONL 翻译域。它理解
[固定 JSONL 契约](jsonl.md)；原游戏格式的识别和译文放回游戏，由外部操作者负责。

适合使用 Generic 的情况包括：

- 游戏引擎没有专用 ATT 支持；
- MV/MZ Builtin 与 Rules 无法完整覆盖某些插件或脚本文本；
- 外部操作者已经能稳定提取、定位并重新写回文本；
- 一批普通文本需要使用 ATT 的语言、术语、Placeholder、全局去重与模型执行能力。

外部操作者或工具负责生成 JSONL 和消费译后 JSONL，转换工具和实现方式由操作者选择；
Group、JSONL 文件与来源结构之间的边界遵守
[从源格式建立 Group 与文件范围](jsonl.md#3-从源格式建立-group-与文件范围)的规则。

## 项目流程

1. [Init](init.md) 绑定外部 JSONL 根和语言对；
2. [Extract](extraction.md) 读取当前 JSONL 并更新项目状态；
3. [Translate](translation.md) 处理当前未译内容；
4. [WriteBack](write-back.md) 生成译后 JSONL。

[JSONL 示例](examples/sample.jsonl)、
[Generic Placeholder 示例](examples/placeholders.toml)和
[Generic 术语示例](examples/terminology.toml)可以直接复制使用；字段与行为以相应现行
规格为准。

Generic 来源可以长期修改。添加、删除、移动或改写内容后，先重新 Extract，再沿用同一个
项目继续工作；Translate 和 WriteBack 始终以最近一次成功 Extract 的内容为准。

同一个游戏的 Generic 项目与 MV/MZ 项目各自独立。先明确每段内容由哪个项目负责，就能
避免重复提取或重复写回。
