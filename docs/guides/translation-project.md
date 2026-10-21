# ATT 游戏翻译项目指南

按以下主线完成翻译，并从最早受影响的阶段继续返修：

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

先建立调查目录，再审核关系组和实际消费者：

```powershell
python skills/translate-with-att/scripts/rpg_maker_survey.py scan `
  --game D:\games\original --output D:\work\survey
```

Builtin 位置由扫描结果单独投影；调查目录中的 `ownership-decisions.jsonl` 默认把待审核位置标为
`unresolved`，审核后改为 `rules`、`generic`、`exclude`，证据不足时保留 `unresolved`。填写决定时
保留 `game_root` 与 `members` 来源绑定；排除项填写 `reason` 和 `evidence`。关系组中存在不同所有者
时，按 `review-groups.jsonl` 中的 `group_id` 导出逐候选决定，以这些行替换原 `group:*` 决定：

```powershell
python skills/translate-with-att/scripts/rpg_maker_survey.py members `
  --survey D:\work\survey --group-id <group_id> --output D:\work\group-members.jsonl
```

Generic 决定的 `extract_group_unit_write_back_mapping.groups` 指定本行候选所属的语义组。
每个候选按本行自然顺序恰好出现一次；不同决定中相关的正文填写相同 id、kind，并由 finalize
合并。例如任务标题与说明分别归 Generic，内部编号归 exclude，两个 Generic 决定分别填写：

```json
{"groups":[{"id":"quest-entry-1","kind":"quest_entry","candidate_ids":["location-000101"]}]}
{"groups":[{"id":"quest-entry-1","kind":"quest_entry","candidate_ids":["location-000102"]}]}
```

同一决定也可列出多个独立组。其余 Generic 证据说明实际消费者、可见正文及往返位置。
组名表达[完整语境](../generic/jsonl.md#3-从源格式建立-group-与文件范围)，不由调查分组或 owner 决定。

决定修改后重新运行 finalize，生成配套的 `rules.toml`、`rules-manifest.json`、`coverage.json`，
以及 `generic/` 下的 Generic 材料和精确映射 `generic/manifest.json`：

```powershell
python skills/translate-with-att/scripts/rpg_maker_survey.py finalize `
  --survey D:\work\survey --output D:\work\plan
```

省略 `--decisions` 时读取调查目录中的 `ownership-decisions.jsonl`；重新生成既有计划目录时加
`--replace`。计划目录由工具生成；直接修改其中的 Rules 会使 audit 报告 Rules 与 manifest 不一致。
扫描候选本身有误时，按翻译 Skill 的工具修复流程处理，再重新生成计划。

自行按[规则规格](../rpg-maker/rules.md)编写的 Rules 或 Placeholder 文件作为独立项目输入维护，
通过 ATT 实际提取、导出与写回核对。修改后的规则不能继续使用未同步的 Survey 计划作为一致性证据。

## 3. 建立项目所有权

### 3.1 MV/MZ 按原生能力顺序判断

MV/MZ 来源按 [Extract](../rpg-maker/extraction.md)和 [Rules](../rpg-maker/rules.md)判断：

1. 先确认 Builtin 覆盖；
2. 再用 Rules 表达已知数据路径、事件参数、插件参数和可逆文本捕获；
3. 最后把 Rules 无法表达且已有外部往返过程的精确来源交给 Generic。

插件源码硬编码文字、跨事件脚本和其他外部来源可以建立 Generic JSONL。外部过程负责稳定 ID、
自然语境、来源映射和译后写回。

### 3.2 Generic 项目

按 [Generic JSONL](../generic/jsonl.md)建立文件、Group 和 Unit。先沿实际剧情核对完整场景、
连续对话、旁白与选项的关系，再确定分组；抽查场景开头、转折和结尾是否一起提供给模型。
独立写回位置保留为 Unit，不能因此把连续剧情拆成独立 Group。记录从真实来源到 JSONL、再到
最终消费者的完整映射。
大批量翻译前，用少量真实替换文本验证原格式写回及原生消费者。封包后的完整观察按
[外部适配器验收](../generic/adapter-verification.md)绑定最终资源并汇入 QA。

### 3.3 组合项目

同一游戏可以组合 MV/MZ 与 Generic。为每类来源记录唯一项目、输入、提取方式、输出、合并顺序
和实际消费者。

## 4. Extract

按 [MV/MZ Init](../rpg-maker/init.md)或 [Generic Init](../generic/init.md)建立项目，再用本轮确定的
Builtin、Rules 或 JSONL 执行 Extract。完成后：

1. MV/MZ 导出 ownership，核对已纳入位置的唯一所有者；使用 Survey 时同步运行 audit。独立
   Generic 核对外部来源到 JSONL 的逐项映射；
2. 处理 Extract 给出的具体位置问题；MV/MZ 同时处理 Rules 诊断；
3. 导出 `--selection all` 的完整 Manual；
4. 把完整 Manual 固定为本轮术语与翻译语料。

Survey 路径的产物按以下依赖交接：

`scan → 审核决定 → finalize → Extract → ownership export → audit → manual export --selection all`

audit 使用本轮 ownership 导出和同一次 finalize 的计划目录：

```powershell
att <mv或mz> ownership export --name <项目名> D:\work\ownership.jsonl
python skills/translate-with-att/scripts/rpg_maker_survey.py audit `
  --survey D:\work\survey --plan D:\work\plan `
  --ownership D:\work\ownership.jsonl --output D:\work\audit.json
```

来源、所有权、Rules、JSONL 或辅助程序变化时，从最早受影响的阶段重跑，更新后续产物、术语与 QA；
仍与当前来源绑定一致的审核决定可继续使用。MV/MZ 游戏来源变化时先再次 Init 更新冻结副本，再 Extract。

需要检查 RPG Maker Placeholder 候选时，在完整 Manual 导出后运行 `translation_preflight.py`：

1. 首次提供本轮 `--manual`、同源 `--survey` 和同一次 finalize 的 `--coverage`，不带 `--decisions`。
   用 `--output` 指定检查目录；重新生成既有目录时加 `--replace`。`rules-manifest.json` 保持在
   `coverage.json` 同目录，供工具核对。
2. 阅读 `placeholder-candidates.jsonl`，根据实际消费者选择候选的保护方式或保留待确认状态。将审核
   JSONL 保存在产物目录之外，再传入 `--decisions`，配合 `--replace` 更新同一个 `--output` 目录。
3. 检查生成的 `preflight.json` 和 `placeholder-rules.toml`，把确认的规则交给 Translate；未解决项
   保留调查入口和验证边界。保护方式变化时修改审核决定并重新生成规则。

Manual、Survey 或 coverage 变化后，先重新运行不带 `--decisions` 的译前检查，再基于新候选审核。

## 5. 术语

使用[游戏术语表制作 Skill](../../skills/extract-game-terminology/SKILL.md)从完整 Manual 生成
`terminology.toml`。随包 Formic 先初筛原文术语候选，这一步不可省略；当前负责翻译的 Agent 再按
该 Skill 在完整 Manual 中核对全部出现位置和上下文，完成筛选、去重、统一定译和最终术语文件核对。
Formic 无法完成初筛时，按该 Skill 暂停术语阶段，修正 Formic 配置或模型服务后继续。

术语完成后按[术语规格](../translation/terminology.md)交给 Translate。

## 6. Translate

按 [MV/MZ Translate](../rpg-maker/translation.md)或
[Generic Translate](../generic/translation.md)运行当前项目。Translate 使用当前配置、术语、
Placeholder 和项目语境生成并保存译文。

命令结束后查看 NoWork、Complete 或 Incomplete 的汇总，并导出当前译文。Incomplete 中的
Partial、Unavailable 和未开始 Task，以及失败或取消的运行，按
[诊断与恢复指南](diagnosis-and-recovery.md#64-translate)处理。当前 Rejected 需要显式重试或
人工修订；少量剩余和语境问题使用 [Manual TOML](../manual/README.md)集中完成。

## 7. QA

按[验收指南](acceptance.md)检查完整译文的来源覆盖、语义、术语、结构、残留与组合项目所有权。
当前负责翻译的 Agent 直接对照原文、译文和相关上下文完成语义审校；`translation_qa.py` 的命令、
证据参数和结果读取顺序见[译文质量检查](acceptance.md#3-检查译文质量)。审核发现后用 Manual
集中修订，再重新导出和复查。

## 8. WriteBack

通过 QA 的当前译文按 [MV/MZ WriteBack](../rpg-maker/write-back.md)或
[Generic WriteBack](../generic/write-back.md)生成输出。排版规则按
[WriteBack 排版规则](../translation/write-back-layout-rules.md)处理已确认的宽度和换行位置。

RPG Maker 输出部署到隔离副本；Generic 输出经过本任务确定的外部反向转换；组合项目按真实加载
顺序合并。核对完整文件集合、解析结果、差异和每个来源的最终译文。

## 9. 字体、封包与人工实机验证

RPG Maker 游戏按[字体工具指南](nwjs-font-tools.md#2-递归字体调查替换与恢复)检查实际字体引用，
在隔离副本中应用中文字体并验证完整译文字符集。其他引擎按其实际字体加载方式处理。
把 WriteBack、外部转换、字体和原有资源组合为独立交付目录。

RPG Maker MV/MZ 隔离副本的启动验收和实际绘制观察见
[NW.js 实际界面观察](nwjs-font-tools.md#1-nwjs-实际界面观察)。静态检查完成后，按
[人工实机验证](acceptance.md#6-人工实机验证)整理检查清单，交给任务发起者指定的人工运行译本；
反馈返修按同一节从最早受影响阶段继续。
