# Generic 项目

Generic 是 ATT 面向任意游戏或文本的 JSONL 翻译域。它理解
[固定 JSONL 契约](jsonl.md)；原游戏格式的识别和译文放回游戏，由外部操作者负责。

没有 ATT 专用支持的游戏引擎、需要批量翻译的普通文本，以及 MV/MZ 原生能力无法完整表达的
来源，都可以使用 Generic。前提是外部操作者能够稳定提取、定位并消费译后文本；需要放回原
格式时，还应具备可逆写回能力。

MV/MZ 项目分配前先按
[翻译项目指南](../guides/translation-project.md#31-mvmz-按原生能力顺序判断)完成原生
能力判断；具体边界只以 MV/MZ Extract 与 Rules 规格为准。

外部操作者或工具负责生成 JSONL 和消费译后 JSONL，转换工具和实现方式由操作者选择；
Group、JSONL 文件与来源结构之间的边界遵守
[从源格式建立 Group 与文件范围](jsonl.md#3-从源格式建立-group-与文件范围)的规则。

## 项目流程

1. [Init](init.md) 绑定外部 JSONL 根和语言对；
2. [Extract](extraction.md) 读取当前 JSONL 并更新项目状态；
3. [Translate](translation.md) 处理当前未译内容；
4. [Manual](../manual/README.md) 导出、检查并应用仍需人工处理的 TOML 条目；
5. [WriteBack](write-back.md) 生成译后 JSONL。

普通人工补译使用 Manual TOML。需要一次批量读取上下文、复杂筛选、批量变换、诊断或
特殊数据库修改时，使用 [Lua](../lua/README.md)。

[JSONL 示例](examples/sample.jsonl)、
[Generic Placeholder 示例](examples/placeholders.toml)和
[Generic 术语示例](examples/terminology.toml)可以直接复制使用；字段与行为以相应现行
规格为准。

Generic 来源可以长期修改。添加、删除、移动或改写内容后，先重新 Extract，再沿用同一个
项目继续工作；Translate 和 WriteBack 始终以最近一次成功 Extract 的内容为准。

同一个游戏的 Generic 项目与 MV/MZ 项目各自独立。先明确每段内容由哪个项目负责，就能
避免重复提取或重复写回。

失败与继续见[诊断与恢复指南](../guides/diagnosis-and-recovery.md)，外部转换、组合输出和
实际消费者见[翻译验收指南](../guides/acceptance.md)。
