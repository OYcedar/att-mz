# ATT 游戏翻译项目指南

这份指南组织一条完整交付主线：

`调查 → Extract → 术语 → Translate → QA → WriteBack → 字体/封包`

命令参数、文件格式和状态语义由各阶段现行规格负责。完整执行入口见
[使用 ATT 完成游戏翻译](../../skills/translate-with-att/SKILL.md)。

## 1. 确认翻译目标

开始时明确：

- 实际 ATT 发行根和 `att.exe`；
- 游戏版本、补丁、DLC、MOD 与原版基线；
- 源语言、目标语言和可见非图片文本范围；
- 译本输出目录、外部转换和最终游戏消费者；
- 由任务发起者指定的人工实机验证范围。

继续已有任务时读取当前项目状态、输入、日志、Manual 和 WriteBack，确定本轮从哪个阶段继续。

## 2. 调查可见文本

为声明范围内每类文本记录来源文件、对象或字段、显示场景、上下文、自然顺序、控制符、写回方式
和消费者。RPG Maker 项目可以使用随 Skill 的 `rpg_maker_survey.py` 生成关系组、所有权决定模板
和 Rules 候选。

每段文本选择唯一所有者：

- Builtin 处理 ATT 原生覆盖的位置；
- Rules 处理能够确定、可逆提取和写回的位置；
- Generic 处理已建立外部 JSONL 往返映射的其他来源。

暂时缺少消费者证据的位置记录为 `unresolved`，并把对应场景加入人工实机调查清单。

## 3. 建立项目所有权

### 3.1 MV/MZ 按原生能力顺序判断

MV/MZ 来源按 [Extract](../rpg-maker/extraction.md)和 [Rules](../rpg-maker/rules.md)判断：

1. 先确认 Builtin 覆盖；
2. 再用 Rules 表达已知数据路径、事件参数、插件参数和可逆文本捕获；
3. 最后把 Rules 无法表达且已有外部往返过程的精确来源交给 Generic。

插件源码硬编码文字、跨事件脚本和其他外部来源可以建立 Generic JSONL。外部过程负责稳定 ID、
自然语境、来源映射和译后写回。

### 3.2 Generic 项目

按 [Generic JSONL](../generic/jsonl.md)建立文件、Group 和 Unit。每个 Group 包含能够共同理解的
自然语境；能够独立翻译和写回的记录使用独立 Group。记录从真实来源到 JSONL、再到最终消费者的
完整映射。

### 3.3 组合项目

同一游戏可以组合 MV/MZ 与 Generic。为每类来源记录唯一项目、输入、提取方式、输出、合并顺序
和实际消费者。

## 4. Extract

按 [MV/MZ Init](../rpg-maker/init.md)或 [Generic Init](../generic/init.md)建立项目，再用本轮确定的
Builtin、Rules 或 JSONL 执行 Extract。完成后：

1. 导出 ownership，核对已纳入位置的唯一所有者；
2. 处理 Rules 和 Extract 给出的具体位置问题；
3. 导出 `--selection all` 的完整 Manual；
4. 把完整 Manual 固定为本轮术语与翻译语料。

来源、所有权、Rules 或 JSONL 变化时重新 Extract，并更新依赖该语料的术语与 QA。

## 5. 术语

使用[游戏术语表制作 Skill](../../skills/extract-game-terminology/SKILL.md)从完整 Manual 生成
`terminology.toml`。Agent 可以直接处理全部语料；大量独立 Scope 可以由随包 Formic 生成候选，
再由 Agent 在完整语料中统一筛选和定译。

术语完成后按[术语规格](../translation/terminology.md)交给 Translate。

## 6. Translate

按 [MV/MZ Translate](../rpg-maker/translation.md)或
[Generic Translate](../generic/translation.md)运行当前项目。Translate 使用当前配置、术语、
Placeholder 和项目语境生成并保存译文。

命令结束后确认实际业务结果并导出当前译文。Partial、Unavailable 或中断任务按
[诊断与恢复指南](diagnosis-and-recovery.md)沿当前状态继续；少量剩余和语境问题使用
[Manual TOML](../manual/README.md)集中完成。

## 7. QA

按[验收指南](acceptance.md)检查完整译文，重点覆盖：

- 全部已声明来源和自然 ID；
- 上下文、人物语气、叙事与目标语自然度；
- 术语、Placeholder、控制符、结构、换行和布局；
- 源语言残留、异常转义和模型说明；
- Generic 外部映射与组合项目的唯一所有权。

使用 `translation_qa.py` 聚合 Review 组，Agent 审核后用 Manual 集中修订，并重新导出和复查。

## 8. WriteBack

通过 QA 的当前译文按 [MV/MZ WriteBack](../rpg-maker/write-back.md)或
[Generic WriteBack](../generic/write-back.md)生成输出。排版规则按
[WriteBack 排版规则](../translation/write-back-layout-rules.md)处理已确认的宽度和换行位置。

RPG Maker 输出部署到隔离副本；Generic 输出经过本任务确定的外部反向转换；组合项目按真实加载
顺序合并。核对完整文件集合、解析结果、差异和每个来源的最终译文。

## 9. 字体、封包与人工实机验证

按[字体工具指南](nwjs-font-tools.md#2-递归字体调查替换与恢复)检查实际字体引用，在隔离副本中应用中文字体并验证
完整译文字符集。把 WriteBack、外部转换、字体和原有资源组合为独立交付目录。

静态检查完成后，把标题、菜单、主要对话、插件界面、换行、裁切、字体回退和存档列入实机检查
清单。任务发起者指定的人工运行译本并反馈具体场景；返修从最早受影响阶段继续，随后重新 QA、
WriteBack 和封包。
