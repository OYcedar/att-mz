---
name: translate-with-att
description: 使用已打包 ATT 完成 RPG Maker MV、MZ、Generic 或组合项目的游戏翻译，从可见文本调查、Extract、术语、Translate、QA、WriteBack 一直到中文字体与封包，并根据人工实玩反馈继续返修。
---

# 使用 ATT 完成游戏翻译

目标是交付可供人工实机验证的译本。主线固定为：

`调查 → Extract → 术语 → Translate → QA → WriteBack → 字体/封包`

ATT 负责确定性提取、状态、模型任务、译文验收和写回；Agent 负责调查文本来源、确定所有者、
制作术语、审校译文和处理游戏特有内容。命令、格式和状态以实际 `att.exe` 同目录的现行文档为准。

## 使用当前发行

1. 确认 `att.exe`、发行根、游戏原版、目标目录、源语言、目标语言和翻译范围。
2. 读取发行根的 `README.md`、`docs/README.md`、
   `docs/guides/translation-project.md`，再读取当前引擎和阶段的规格。
3. 继续已有项目时，以 ATT 数据库、当前输入、日志和已有译文为续跑依据。
4. 原版游戏作为只读基线；Extract、字体应用、合并和封包使用 ATT 项目目录或隔离副本。

当 RPG Maker MV 项目同时具有数万翻译单元、大量活动插件、嵌套插件参数和多种玩家界面时，
读取[大型、高插件 MV 经验](references/game-type-large-plugin-heavy-mv.md)。

## 1. 调查

建立声明范围内的可见非图片文本清单，记录每类文本的来源、游戏消费者、上下文、写回位置和
唯一所有者。图片文字交给图像翻译流程，资源路径、内部键、控制符和协议外壳保留其技术含义。

RPG Maker 项目优先使用随包 `rpg_maker_survey.py` 调查标准数据、事件、活动插件参数、插件源码
和自定义数据。根据真实结构把来源交给：

- Builtin：ATT 原生覆盖的位置；
- Rules：能够确定、可逆提取和写回的位置；
- Generic：已经建立外部 JSONL 往返映射的其他可见文本来源。

随包 Python 程序属于 ATT 统一维护的可执行工具。普通翻译任务使用当前发行副本，或项目发布者提供
的完整替换文件。程序输出的消费者推断、关系分组、Rules 和 Placeholder 建议都是候选；Agent 用
真实游戏消费者以及能区分边界的正反例审核候选，再写入当前项目决策。`analysis_status=confirmed`
只确认扫描所得的结构观察，玩家可见正文边界和最终所有者仍由消费者证据确定。

项目选择或消费者证据有误时，修改生成的 decisions、Rules 和 Placeholder Rules。Manual 只编辑
译文字段；Manual ID、scan/finalize 产物以及 audit/preflight 结果使用相应工具重新生成，以生成结果
完成对账。

按[项目调查指南](../../docs/guides/translation-project.md#2-调查可见文本)填写决定并保留来源绑定。
所有权与翻译语境分别判断：排除内部键后，相关标题、说明或对白仍可通过同一组名一起翻译；
独立记录使用不同组名。用同一次 finalize 产物继续 Extract、audit 与 preflight。

暂时缺少消费者证据的位置标记为 `unresolved`，并列入人工实机检查清单。用户提供的截图、场景和
触发步骤可以用于补充消费者证据和定位遗漏来源。

## 2. Extract

按引擎规格执行 Init，保存本轮实际采用的 dialogue rules、Extract Rules 和 Generic 输入，然后
执行 Extract。导出 ownership 并运行 Survey audit，确认每个已纳入位置只有一个所有者。

Survey 生成的 Manual ID、ownership 投影、audit 和 preflight 集合必须与同目录 `att.exe` 的实际
导出逐项一致。出现缺失、多出或不匹配时，以首个不一致的物理位置为反例，追溯分类和编号规则，
并判断 Survey 或 `att.exe` 哪一侧偏离现行规格。`att.exe` 符合现行规格而 Survey 偏离时，维护者在
仓库统一源 `skills/translate-with-att/scripts` 修复脚本、验证受影响来源并通过发行资源同步交付；
普通任务继续使用完整发行文件，不建立游戏私有脚本分支。`att.exe` 与现行规格冲突时，暂停翻译
流水线并在 ATT 语义所有者处修复根因，脚本随后对齐修复后的正确投影。

辅助程序更新后，从最早受影响的 Survey 阶段重新生成 decisions、Rules、Placeholder Rules、Manual
ID、audit 和 preflight 结果，然后重新执行 Extract、ownership export、audit 和 preflight。

Extract 完成后导出完整 Manual：

```powershell
att <mv或mz或generic> manual export --name <项目名> --selection all <工作目录>\final-manual.toml
```

这份 Manual 是本轮术语和翻译的完整语料。来源、Rules、所有权或 Extract 发生变化时，重新导出
Manual，并用新语料更新术语和后续 QA。

需要检查 RPG Maker Placeholder 候选时，在 Translate 前运行随包
`translation_preflight.py`，把确认的保护规则交给 ATT。

## 3. 术语

读取 `skills/extract-game-terminology/SKILL.md`，从完整 Manual 制作最终 `terminology.toml`。
Agent 可以直接完成全部语料；大量独立语料分片也可以交给随包 Formic 生成候选，再由 Agent
统一筛选和定译。最终术语文件由 ATT `translate --terms` 读取。

## 4. Translate

使用当前 ATT 配置、术语和 Placeholder 运行对应引擎的 Translate。命令结束后按 Translate 规格
确认 Complete、Partial 或 Unavailable 的实际含义，并导出当前译文：

```powershell
att <mv或mz或generic> translation export --name <项目名> <工作目录>\translations.jsonl
```

可恢复任务沿当前项目状态继续。少量剩余、语境歧义和已经定位的质量问题使用 Manual TOML
集中补译或修订，使 Agent 能直接完成最后的自然单元。

## 5. QA

按 `docs/guides/acceptance.md` 检查完整译文。使用随包 `translation_qa.py` 汇总以下内容：

- 可见文本覆盖和所有权；
- 目标语自然度、上下文、人物语气和叙事一致性；
- 术语、专名和系统用语一致性；
- Placeholder、控制符、空槽、结构和换行；
- 源语言残留、模型说明、异常转义和布局风险。

先按 Review 组审核问题，再导出对应自然 ID 的 Manual，集中修订、apply、重新导出并复查。QA
结果清楚区分已经静态确认的范围和需要人工实机观察的场景。

## 6. WriteBack

QA 修订完成后执行对应引擎的 WriteBack。已经确认具体位置和显示宽度时使用排版规则；规则文件
按 `docs/translation/write-back-layout-rules.md` 编写。

RPG Maker 输出部署到隔离游戏副本。Generic 输出交给本任务已经确定的外部反向转换，并核对每个
JSONL Unit 与实际来源位置。组合项目按真实加载顺序合并，确保每个位置采用唯一译文。

## 7. 字体与封包

使用 `manage_rpg_maker_fonts.py inspect` 检查实际字体引用，再在隔离副本中执行 `apply`。根据当前
完整译文字符集确认中文 glyph 覆盖，并保留游戏运行时使用的字体名称和加载关系。可选字体包括
随 Skill 提供的 Noto Sans CJK SC、Noto Serif CJK SC 和霞鹜文楷 GB。

封包时汇总 WriteBack、Generic 外部结果、字体和游戏原有资源，生成独立交付目录。交付目录完成
结构解析、可见文本残留、字体覆盖和启动文件检查后，向任务发起者提供人工实机检查清单。实机
验证由任务发起者指定的人工完成，重点覆盖标题、菜单、主要对话、插件界面、换行、裁切和存档。

## 实玩反馈返修

收到截图、原文、场景和触发步骤后，先定位真实来源和所有者，再从最早受影响的阶段继续：

- 新来源进入调查、所有权和 Extract；
- 术语变化更新术语表并审校受影响译文；
- 误译和排版问题进入 Manual、QA、WriteBack 与重新封包；
- 字体问题进入字体引用、glyph 覆盖和相关场景复查。

每轮交付说明译本目录、覆盖范围、静态 QA 结果、人工实机检查项和仍待确认的翻译位置。

## 状态恢复

命令失败、Partial、Unavailable、取消或状态不明时，读取
`docs/guides/diagnosis-and-recovery.md`，根据当前项目状态选择恢复动作。恢复沿用现有项目、输入、
术语、Manual 和 WriteBack 结果，使已经确认的翻译继续成为后续工作的基础。
