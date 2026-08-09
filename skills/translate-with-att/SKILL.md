---
name: translate-with-att
description: 使用 ATT 调查、建立、继续、诊断、修订、写回和验收 RPG Maker MV、MZ、Generic 或组合式游戏翻译。适用于用户明确要求使用 ATT、提供 ATT 项目，或要求处理 Init、Extract、Rules、Translate、Manual、Lua、WriteBack、运行错误和恢复。
---

# 使用 ATT 翻译游戏

本 Skill 固定必须得到的结果，不固定 Agent 的执行顺序。命令、格式和错误语义以本次
`att.exe` 同目录的现行文档为准；随 Skill 的 Python 程序只负责加速调查和生成审核材料。

## 先绑定本次发行

1. 确认实际 `att.exe`、发行目录和调用 cwd，完整读取同目录 `README.md`、
   `docs/README.md` 与 `docs/guides/translation-project.md`。
2. 失败、不完整、取消或状态不明时读 `docs/guides/diagnosis-and-recovery.md`；进入审校、
   WriteBack 或交付时读 `docs/guides/acceptance.md`。
3. 再按阶段读取 MV/MZ、Generic、语言、Placeholder、术语、配置和运行时专题规格。固定资源
   缺失或文档互相冲突时停止，不从其他安装或旧对话拼接替代内容。

Agent 可以主动把互不写同一文件的只读扫描、运行记录分析和候选审核交给 subagent。
是否使用、如何拆分和调用顺序由 Agent 根据游戏规模决定。ATT 项目、重叠文件和最终规则
始终只有一个写入者。

## 必须得到的结果

常见 MV/MZ 任务必须完成以下检查：

1. 识别真实游戏根、内容根、MV/MZ、活动插件、Builtin 覆盖和全部其他文本来源。
2. MV 调查实际姓名框协议，保存有效 dialogue rules 或明确 `rule = []`；MZ 使用原生
   Speaker，不制作 MV 姓名规则。
3. Builtin 之后调查 Extract Rules，保存审核后的规则或明确 `rule = []`。
4. 用最终 Extract 的完整 Manual 调查 Placeholder，保存审核后的规则或明确 `rule = []`。
5. 审计每个非 Builtin 来源的唯一所有者：Rules、已证实的 Generic、有依据的排除或未确认。
   尚有未确认来源时不得声称覆盖完整。
6. 首次 Translate 前从同一份最终 Manual 制作术语表；术语允许为空。
7. Translate 后确认无需处理、完整或未完整；WriteBack 后检查输出、JSON 可读性和游戏原件
   未被修改。

资源字段和整值资源路径只是资源引用，不是玩家文本，不进入 Rules 候选或术语候选，也绝不
写入 `allowed_terms`。`allowed_terms` 只列玩家可见译文中确实需要保留的源语片段。自然句中
提到扩展名不因此成为资源引用；`.txt`、`.json` 和 `.js` 也只是容器类型，不能按后缀排除。

## Generic 默认关闭

只有一个精确来源同时具备以下事实时才启用 Generic：

1. 位于游戏目录内，有精确自然位置，且不是图片文字；
2. 当前游戏确实启用了读取它的运行时消费者；
3. 有证据证明它会在正常游玩中向玩家显示；
4. Builtin 不覆盖，Rules 也无法完整、确定、可逆地读取和写回；
5. 已确定提取、Group/Unit、稳定 ID 和译后写回映射；
6. 不与 MV/MZ 或其他 Generic 项目重复拥有同一文本。

文件存在、补丁说明存在、插件复杂、源码出现引用或“可能显示”都不够。只纳入通过审核的
精确来源，不递归翻译整个目录或文件类型。静态 JavaScript 单字面量替换和分段纯文本往返
也必须先通过上述审核；共享助手只执行 Agent 已批准的精确替换，不会自动建立 Generic 项目。

## 最短工具流程

先确认 Python 3.11+。可用时优先使用以下标准库程序；缺少 Python 时按现行文档人工调查，
不自动安装。输出默认拒绝覆盖，确认可替换时才加 `--replace`。
盘点和来源追踪优先传完整游戏安装根；直接传 `www` 时只会在父级 `Game.exe` 与本目录
`package.json` 同时存在的标准 Windows MV 布局中安全识别父安装根，其他内容根会要求改传安装根。

```powershell
python <Skill>\scripts\inspect_rpg_maker.py --game <游戏根> --output <工作目录>\inventory.json
```

MV 姓名框先生成候选，再用 Agent 审核 JSON 写规则；MZ 跳过：

```powershell
python <Skill>\scripts\analyze_mv_dialogue.py --game <游戏根> --output <工作目录>\dialogue-candidates.json
python <Skill>\scripts\analyze_mv_dialogue.py --game <游戏根> --output <工作目录>\dialogue-candidates.json --decisions <审核.json> --rules-output <dialogue-rules.toml>
```

Rules 也先生成候选；审核写入时必须同时保存与当前 TOML 逐条对应的自然 manifest：

```powershell
python <Skill>\scripts\analyze_extract_rules.py --game <游戏根> --output <工作目录>\rules-candidates.json
python <Skill>\scripts\analyze_extract_rules.py --game <游戏根> --output <工作目录>\rules-candidates.json --decisions <审核.json> --inventory <工作目录>\inventory.json --rules-output <rules.toml> --manifest-output <rules-manifest.json>
```

按发行文档完成 Init 和 Extract 后，从同一快照同时导出 Manual 与 owner：

```powershell
att <mv或mz> manual export --ownership <ownership.jsonl> --name <项目名> <final-manual.toml>
python <Skill>\scripts\audit_text_ownership.py --inventory <inventory.json> --ownership <ownership.jsonl> --rules <rules.toml> --rules-manifest <rules-manifest.json> --decisions <所有者审核.json> --output <ownership-audit.json>
```

审计程序按 Manual 自然顺序使用 `manual_id`、`owner` 和 Rules 的自然 `rule_number`，并验证
manifest 与当前 Rules TOML 逐条一致。不要从 ID 前缀猜 owner，也不要读取 SQLite。

最终 Manual 确定后再生成 Placeholder 候选并写审核后的规则：

```powershell
python <Skill>\scripts\analyze_placeholders.py --manual <final-manual.toml> --output <placeholder-candidates.json>
python <Skill>\scripts\analyze_placeholders.py --manual <final-manual.toml> --output <placeholder-candidates.json> --decisions <审核.json> --rules-output <placeholder-rules.toml>
```

术语制作读取 `skills/extract-game-terminology/SKILL.md`。指定外部来源需要补充 Generic 证据时
运行 `trace_runtime_text.py --help`；Translate 日志汇总和 WriteBack 校验分别运行
`summarize_att_run.py --help` 与 `verify_write_back.py --help`。

## 失败与恢复

- Python 程序返回非零时，按 stderr 的对象、原因、影响和处理办法修正输入；不得跳过失败
  输出继续，也不得把候选当最终规则。
- ATT 失败、Partial、Unavailable、取消或结果未知时，读取同次项目 JSONL 和相应恢复规格；
  只执行该原因对应的恢复方法。
- Formic 术语任务中断时，保留同一 OUT、input、plan 和 task，按术语 Skill 的命令加
  `--resume`；不要删除已经发布的 `results`。
- 文档、Skill 和有效配置都无法解释或解决已观察到的问题时，立即停止，说明对象、直接
  原因、已完成状态和继续所缺的事实。

完成时按全量验收指南检查声明范围、每个项目、全部输出和实际消费者。一次成功退出、一次
Complete、输出目录存在或抽样检查都不能单独证明整个游戏已经翻译完成。
