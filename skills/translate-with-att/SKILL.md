---
name: translate-with-att
description: 使用已打包 ATT 完成 RPG Maker MV、MZ、Generic 或组合项目的游戏翻译，从可见文本调查、Extract、术语、Translate、QA、WriteBack 一直到中文字体与封包，并根据人工实玩反馈继续返修。
---

# 使用 ATT 完成游戏翻译

交付可供人工实机验证的译本，按以下主线推进：

`调查 → Extract → 术语 → Translate → QA → WriteBack → 字体/封包`

ATT 负责确定性提取、状态、模型任务、译文验收和写回；Agent 负责调查文本来源、确定所有者、
制作术语、审校译文和处理游戏特有内容。命令、格式和状态以实际 `att.exe` 同目录的现行文档为准。

## 使用当前发行

1. 确认 `att.exe`、发行根、游戏原版、目标目录、源语言、目标语言和翻译范围。
2. 读取发行根的 `README.md`、`docs/README.md`、
   `docs/guides/translation-project.md`，再读取当前引擎和阶段的规格。
3. 继续已有项目时，以 ATT 数据库、当前输入、日志和已有译文为续跑依据。
4. 原版游戏作为只读基线；Extract、字体应用、合并和封包使用 ATT 项目目录或隔离副本。

随包 Python 工具在 Python 3.11 或更新环境中运行。没有 Python 时，按相应指南完成调查和对账，
并在交付中说明实际采用的方法。

RPG Maker MV 项目出现混合插件参数、内联姓名控制码、组合写回或大量 QA 候选时，
读取[大型、插件密集的 MV 经验](references/game-type-large-plugin-heavy-mv.md)中对应的处理方法。

Ren'Py 项目通过 Generic JSONL 翻译时，读取 [Ren'Py 汉化技能](../renpy-localization/SKILL.md)，
补充原生翻译模板、硬编码界面、反向转换、字体与独立补丁的检查。

发现 TyranoScript 场景与 Electron `app.asar` 容器时，读取
[TyranoScript／Electron ASAR 经验](references/engine-tyrano-electron-asar.md)，
补充显示消费者、Generic 映射、标签拓扑、字体、流式回填和独立补丁的检查。

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

项目选择或消费者证据有误时，修改所有权或 Placeholder 的审核决定，再由 finalize 或 preflight
重新生成规则与报告。自行编写的规则按对应规格维护；Survey 产物的更新和核对方式见下方项目调查指南。
Manual 只编辑译文字段，条目 ID 和原文通过 ATT 重新导出。

使用 RPG Maker Survey 时，按[项目调查指南](../../docs/guides/translation-project.md#2-调查可见文本)
填写决定并保留来源绑定，再用同一次 finalize 产物继续 Extract、audit 与 preflight。
独立 Generic 项目按 JSONL 规格建立外部来源映射。两种路径都分别判断所有权与翻译语境：排除
内部键后，相关标题、说明或对白仍可组成同一语义组；独立记录使用不同组。

暂时缺少消费者证据的位置标记为 `unresolved`，并列入人工实机检查清单。用户提供的截图、场景和
触发步骤可以用于补充消费者证据和定位遗漏来源。

## 2. Extract

按引擎规格执行 Init 和 Extract：MV/MZ 使用本轮确定的 Builtin、Extract Rules 及适用的 MV
对话姓名规则；Generic 读取项目绑定的 JSONL 输入。

MV/MZ 导出 ownership，使用 Survey 的项目继续运行 audit，核对每个位置的唯一所有者。
独立 Generic 核对 JSONL 与外部来源、自然顺序和写回位置的映射，不执行 RPG Maker ownership
或 Survey 命令。

使用 Survey 时，生成的 Manual ID、ownership 投影、audit 和 preflight 集合必须与同目录 `att.exe` 的实际
导出逐项一致。出现缺失、多出或不匹配时，以首个不一致的物理位置为反例，追溯分类和编号规则，
并判断 Survey 或 `att.exe` 哪一侧偏离现行规格。`att.exe` 符合现行规格而 Survey 偏离时，维护者在
仓库统一源 `skills/translate-with-att/scripts` 修复脚本、验证受影响来源并通过发行资源同步交付；
普通任务继续使用完整发行文件，不建立游戏私有脚本分支。`att.exe` 与现行规格冲突时，暂停翻译
流水线并在 ATT 语义所有者处修复根因，脚本随后对齐修复后的正确投影。

Survey 辅助程序更新后，按[项目指南中的产物依赖](../../docs/guides/translation-project.md#4-extract)
从最早受影响阶段重新生成后续产物。audit 使用本轮 ownership 导出，preflight 使用本轮完整 Manual 和计划。

Extract 完成后导出完整 Manual：

```powershell
att <mv或mz或generic> manual export --name <项目名> --selection all <工作目录>\final-manual.toml
