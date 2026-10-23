# ATT 翻译验收指南

这份指南用于 Translate 后 QA、WriteBack 检查、字体与封包确认，以及人工实机反馈返修。验收围绕实际译文、输出和玩家可见结果展开。

## 1. 确认验收对象

明确游戏版本、语言、声明范围、每类文本的唯一项目所有者、当前输入、最终输出目录和人工实机检查范围。组合项目同时明确各项目的合并顺序和最终消费者。

## 2. 检查文本覆盖

核对调查范围、完整 Manual 和当前 `translation export`（MV/MZ 同时核对 ownership）：

- 每个已声明可见文本来源进入正确项目，且只交给一个提取与写回流程；
- 每个 Group 和 Unit 保留所需上下文、自然顺序和稳定来源映射；
- 每个待译自然文本拥有当前译文或明确的原文保留理由；
- 待确认来源和场景列入人工检查清单。

Generic 项目同时核对外部来源、JSONL、WriteBack 和实际消费者之间的映射；独立 Generic 可按[外部适配器验收](../generic/adapter-verification.md)生成验证报告，通过 `translation_qa.py scan --adapter-report` 汇入 QA。

## 3. 检查译文质量

对完整当前译文检查：

- 原意、上下文、人物关系、人称、语气和叙事一致性；
- 角色、地点、组织、物品、技能和系统术语；
- Placeholder、RPG Maker 控制符、插件协议、数字和空槽；
- 标点、换行、数组形状、UI 长度和可读性；
- 源语言残留、模型说明、JSON 痕迹和异常转义。

当前负责翻译的 Agent 按[翻译 Skill 的 QA 流程](../../skills/translate-with-att/SKILL.md#5-qa)直接核对原意、上下文、语气和自然度；随 Skill 的 `translation_qa.py scan` 读取完整导出和同源证据，提供覆盖、结构、控制符、字面术语、残留和布局风险的静态检查。

RPG Maker 项目和经 Survey 接入的 Generic 项目提供同一来源快照的调查目录，以及同一次 `finalize` 的 `coverage.json`（`rules-manifest.json` 保持在 `coverage.json` 同目录）：

```powershell
python skills/translate-with-att/scripts/translation_qa.py scan `
  --translations D:\work\translations.jsonl `
  --survey D:\work\survey --coverage D:\work\plan\coverage.json `
  --terminology D:\work\terminology.toml `
  --write-back <att-dir>\projects\<mv或mz>\<项目名>\write_back `
  --runtime-report D:\review\nwjs-smoke\report.json `
  --output D:\work\qa
```

独立 Generic 改用 `--generic-input` 指定当前 JSONL 输入根，命令见[外部适配器验收](../generic/adapter-verification.md#4-汇入-qa-与返修)。其余证据按实际产物提供，重新生成既有 QA 目录时加 `--replace`：

| 参数 | 证据 | 省略时 |
| --- | --- | --- |
| `--terminology` | 本次 Translate 使用的 `terminology.toml`，启用字面术语检查 | 不检查术语，也不记录未验证项 |
| `--write-back` | ATT 当前项目的实际 `write_back` 目录 | `write_back_output_missing` |
| `--generic-manifest` | 仅 Survey 模式的 Generic 项目：`finalize` 生成的 `generic/manifest.json` | `generic_manifest_missing` |
| `--runtime-report` | 仅 Survey 模式：隔离副本的 NW.js `smoke` 或 `observe` 报告 | `runtime_observation_missing` |
| `--adapter-report` | 仅独立 Generic：外部验收报告，同时需要 `--write-back` | 外部映射、反向转换和实际消费者保持未验证 |

Survey 模式提供 `--write-back` 或 `--runtime-report` 时，QA 先确认原版游戏仍与调查基线一致。RPG Maker 的运行报告还要与 `--write-back` 绑定：观察副本中的写回文件必须与该目录逐字节一致，未写回的调查来源与基线一致；缺少 `--write-back` 时记录 `runtime_write_back_binding_missing`。运行观察的方法见 [NW.js 实际界面观察](nwjs-font-tools.md#1-nwjs-实际界面观察)。

QA 消费完整导出的项目语言对、语言 Review 和源文绑定后的自然文本投影。同一 Generic 文件内的相邻 Unit，以及其他项目同一真实 Group 内的 Unit，参与相邻内容重叠及新增独立引语段检查；日译中还检查共享汉字与术语锚点所提示的错位、明显扩写。选择该 Review 组生成 Manual ID 时包含相邻语境，修订由 Agent 对照原文确认。

没有实际布局测量时标记 `layout_unverified`。外部报告可提供绑定最终显示文本的像素测量；测量未覆盖的条目保留未验证。

按以下顺序读取静态 QA 结果：

1. 查看 `qa-summary.json` 的 `qa_status`、`counts`、`revision_ids` 和 `unverified`，明确当前发现与验证范围。
2. 从 `findings.jsonl` 读取 `analysis_status=confirmed_fact` 的记录，核对未翻译、Rejected、空译文或结构不一致等确定状态。
3. 按 `unverified` 指出的缺口补充来源、写回或外部映射检查；需要实际界面观察的部分进入人工检查清单。
4. 查看 `review-groups.jsonl`，按原因展开审核 `heuristic_review` 发现。

将确认需修订的问题与语义审校发现合并，按自然 ID 导出到 Manual，集中修订、apply、重新导出并复查。若使用 `translation_qa.py manual` 生成候选 ID 文件，它默认包含 `revision_ids`，可通过重复传入 `--review-group` 添加已审核组中的 ID。

## 4. 检查 WriteBack

从当前项目状态重新生成 WriteBack，并检查：

- 输出文件集合和目录结构符合当前引擎，JSON、JavaScript 和其他目标格式能够正常解析；
- 译文、控制符、换行和排版规则出现在对应来源位置；
- 原版基线、用户存档和声明范围外资源保持原值；
- Generic 外部反向转换覆盖全部 JSONL Unit，且替换文本后的控制段、显示映射、编码容量和资源查找符合实际消费者；
- 组合项目按真实加载顺序合并，每个位置采用唯一译文。

把最终输出部署到隔离游戏副本，后续字体与封包都基于这个副本进行。

### 4.1 排版语义与最终显示验收

批量排版前，确定不同文本区域的首行、续行、段落和特殊版式要求：

- 区分有段落、停顿、分页或语法用途的换行，与原版仅用于视觉定宽折行的空白和换行；确认只用于视觉排版的空白和断行，才按目标语言的约定重建。
- 明确 ATT WriteBack、外部适配器和游戏引擎各自的排版分工，由最后决定显示行的阶段统一落实缩进、断行和分页。ATT 对已有 LF、规则断行和续行补空白的处理见 [WriteBack 排版规格](../translation/write-back-layout-rules.md#4-断行与补空白)。
- 外部排版以正文和语义段落作为可重排输入，避免把上一轮生成的显示折行当作下一轮不可拆的硬段落。

先用代表样本（长旁白、多行对话、含控制符或变量的文本、接近宽高边界的段落）核对以下要点，再用于全量处理：

- 首行与自动续行的起点对齐，语义段落、强制换行和分页保持完好，缩进不重复叠加；
- 断点符合标点禁则与阅读停顿，完整文字、缩进及变量展开后的宽高在可用区域内。

## 5. 检查字体与封包

RPG Maker 游戏用 `manage_rpg_maker_fonts.py apply` 扫描实际引用，并把选定中文字体应用到隔离副本（只读调查使用 `inspect`）。其他引擎采用其字体加载方式，根据完整译文检查 glyph 覆盖并保留字体名称与加载关系。

封包目录包含当前 WriteBack、Generic 外部结果、字体、原有运行文件和所需资源。

### 5.1 运行程序改动的影响分析与验收

汉化需要解包重建、修改游戏 EXE／DLL、启动初始化、脚本解释器或资源加载逻辑时，保持原有剧情控制、图像、动画和音频行为：

- **追踪状态及其使用方**：查明被修改的初始化或解释路径写入了哪些状态、何时生效和重置，以及后续读取方（初始化值可能参与脚本解码、参数长度、资源选择或时序）。
- **核对修改后的执行语义**：依据最终程序的实际读取逻辑，检查受影响指令的参数宽度、游标前进、下一条指令边界、跳转目标，以及相关资源查找、显示提交和计时关系。
- **安排代表场景检查**：正常剧情、回想和读档可能采用不同初始化路径，按实际受影响的入口安排静态检查与人工实机检查项。

## 6. 人工实机验证

实机验证由任务发起者指定的人工完成。检查清单至少覆盖：

- 标题、菜单、设置和存档界面；
- 从标题开始新游戏、进入首段剧情，以及本次修改的场景入口与返回路径；
- 主要角色对话、姓名框、选择项和剧情事件；
- 地图、战斗、物品、技能和插件界面；
- 中文字体、缺字、回退字体、换行、裁切和溢出；
- 本轮调查中标记为 `unresolved` 的场景。

Agent 根据人工反馈定位来源与所有者，从最早受影响阶段继续返修。

## 7. 交付说明

交付时简明说明：

- 译本或补丁目录；
- 已覆盖的文本范围及静态 QA、WriteBack、字体处理结果；
- 交给人工实机检查的具体场景和待确认位置。
