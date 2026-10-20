# ATT 翻译验收指南

这份指南用于 Translate 后 QA、WriteBack 检查、字体与封包确认，以及人工实机反馈返修。验收
围绕实际译文、输出和玩家可见结果展开。

## 1. 确认验收对象

记录游戏版本、语言、声明范围、每类文本的唯一项目所有者、当前输入、最终输出目录和人工实机
检查范围。组合项目同时记录各项目的合并顺序和最终消费者。

## 2. 检查文本覆盖

对调查清单、完整 Manual 和当前 `translation export` 进行对应检查；MV/MZ 同时核对 ownership：

- 每个已声明可见文本来源进入正确项目；
- 每个位置只交给一个提取与写回流程；组合项目明确 Builtin、Rules 与 Generic 的边界；
- 每个 Group 和 Unit 保留所需上下文、自然顺序和稳定来源映射；
- 每个待译自然文本拥有当前译文或明确的原文保留理由；
- 待确认来源和场景形成具体人工检查项。

Generic 项目同时核对外部来源、JSONL、WriteBack 和实际消费者之间的逐项映射。
独立 Generic 可按[外部适配器验收](../generic/adapter-verification.md)生成文件绑定的验证报告，
通过 `translation_qa.py scan --adapter-report` 汇入当前 QA；最终游戏或验证程序变化后重新取证。

## 3. 检查译文质量

对完整当前译文检查：

- 原意、上下文、人物关系、人称、语气和叙事一致性；
- 角色、地点、组织、物品、技能和系统术语；
- Placeholder、RPG Maker 控制符、插件协议、数字和空槽；
- 标点、换行、数组形状、UI 长度和可读性；
- 源语言残留、模型说明、JSON 痕迹和异常转义。

当前负责翻译的 Agent 按[翻译 Skill 的 QA 流程](../../skills/translate-with-att/SKILL.md#5-qa)
直接核对原意、上下文、语气和自然度；随 Skill 的 `translation_qa.py scan` 读取完整导出和
同源证据，提供覆盖、结构、控制符、字面术语、残留和布局风险的静态检查。脚本没有报告问题，
只能说明已执行的检查没有发现，不能代替完整语义审校。

RPG Maker 项目和经 Survey 接入的 Generic 项目提供同一来源快照的调查目录，以及同一次 finalize
的 `coverage.json`；`rules-manifest.json` 保持在 `coverage.json` 同目录：

```powershell
python skills/translate-with-att/scripts/translation_qa.py scan `
  --translations D:\work\translations.jsonl `
  --survey D:\work\survey --coverage D:\work\plan\coverage.json `
  --terminology D:\work\terminology.toml `
  --write-back <att-dir>\projects\<mv或mz>\<项目名>\write_back `
  --runtime-report D:\review\nwjs-smoke\report.json `
  --output D:\work\qa
```

独立 Generic 改用 `--generic-input` 指定当前 JSONL 输入根，命令见
[外部适配器验收](../generic/adapter-verification.md#4-汇入-qa-与返修)。其余证据按实际产物提供，
重新生成既有 QA 目录时加 `--replace`：

| 参数 | 证据 | 省略时 |
| --- | --- | --- |
| `--terminology` | 本次 Translate 使用的 `terminology.toml`，启用字面术语检查 | 不检查术语，也不记录未验证项 |
| `--write-back` | ATT 当前项目的实际 `write_back` 目录 | `write_back_output_missing` |
| `--generic-manifest` | 仅 Survey 模式的 Generic 项目：finalize 生成的 `generic/manifest.json` | `generic_manifest_missing` |
| `--runtime-report` | 仅 Survey 模式：隔离副本的 NW.js `smoke` 或 `observe` 报告 | `runtime_observation_missing` |
| `--adapter-report` | 仅独立 Generic：外部验收报告，同时需要 `--write-back` | 外部映射、反向转换和实际消费者保持未验证 |

Survey 模式提供 `--write-back` 或 `--runtime-report` 时，QA 先确认原版游戏仍与调查基线一致。
RPG Maker 的运行报告还要与 `--write-back` 绑定：观察副本中的写回文件必须与该目录逐字节一致，
未写回的调查来源与基线一致；缺少 `--write-back` 时记录 `runtime_write_back_binding_missing`。
运行观察的方法见 [NW.js 实际界面观察](nwjs-font-tools.md#1-nwjs-实际界面观察)。

QA 消费完整导出的项目语言对、语言 Review 和源文绑定后的自然文本投影，不重复猜测语言或
保护段。同一 Generic 文件内的相邻 Unit，以及其他项目同一真实 Group 内的 Unit，参与相邻
内容重叠及新增独立引语段检查；日译中还检查共享汉字与术语锚点所提示的错位、明显扩写。
提示附自然 ID 和前后原译对照，属于启发式审校；单纯在同一引语内重新分行不触发引语提示。
选择该 Review 组生成 Manual ID 时包含相邻语境，修订仍由 Agent 对照原文确认。

没有实际布局测量时标记 `layout_unverified`，不按文件名和固定字符数批量推断溢出。外部报告
可提供绑定最终显示文本的像素测量；测量没有覆盖的条目继续保留未验证。重复的源语提示不在
WriteBack 再报一份；写回额外引入的可疑源文片段仍单独报告。

按以下顺序读取静态 QA 结果：

1. 查看 `qa-summary.json` 的 `qa_status`、`counts`、`revision_ids` 和 `unverified`，明确当前发现
   与验证范围。`needs_review` 表示存在发现；`unverified` 表示仍缺验证；`clean` 只覆盖本次已执行检查。
2. 从 `findings.jsonl` 读取 `analysis_status=confirmed_fact` 的记录，核对未翻译、Rejected、空译文
   或结构不一致等确定状态。结合语言判断和实际消费者，区分需要修订的条目与有依据保留原文的条目。
3. 按 `unverified` 指出的缺口补充来源、写回或外部映射证据；需要实际界面观察的部分进入人工检查清单。
4. 查看 `review-groups.jsonl`，按原因选择并展开需要审核的组。这里仅汇总 `heuristic_review` 发现；
   Review 组为零仍可能存在确定状态或未验证项。

将确认需修订的问题与语义审校发现合并，按自然 ID 导出到 Manual，集中修订、apply、重新导出并复查。
若使用 `translation_qa.py manual` 生成候选 ID 文件，它默认包含 `revision_ids`，可通过重复传入
`--review-group` 添加已审核组中的 ID；只有来源位置或运行场景的发现仍需回到相应调查与验证步骤。

## 4. 检查 WriteBack

从当前项目状态重新生成 WriteBack，并检查：

- 输出文件集合和目录结构符合当前引擎；
- JSON、JavaScript 和其他目标格式能够解析；
- 译文、控制符、换行和排版规则出现在对应来源位置；
- 原版基线、用户存档和声明范围外资源保持原值；
- Generic 外部反向转换覆盖全部 JSONL Unit；
- 修改文本后的原生控制、显示映射、编码容量和资源查找符合实际消费者，不能只做无修改往返；
- 组合项目按真实加载顺序合并，每个位置采用唯一译文。

把最终输出部署到隔离游戏副本，后续字体与封包都基于这个副本进行。

## 5. 检查字体与封包

RPG Maker 游戏用 `manage_rpg_maker_fonts.py apply` 扫描实际引用，并把选定中文字体应用到隔离副本；
需要修改前比较或只读调查时使用 `inspect`。其他引擎采用其字体加载方式。根据完整译文检查 glyph 覆盖，
同时保留引擎和插件使用的字体名称与加载关系。

封包目录包含当前 WriteBack、Generic 外部结果、字体、原有运行文件和所需资源。完成文件解析、
源语言残留、字体覆盖和启动文件检查后，记录交付目录与静态 QA 结果。

### 5.1 运行程序改动的影响分析与验收

汉化需要解包重建、修改游戏 EXE／DLL、启动初始化、脚本解释器或资源加载逻辑时，在改动前
确定受影响的状态和消费者，改动后验证这些路径的执行结果。游戏侧适配与补丁应保持原有剧情
控制、图像、动画和音频行为；分析范围由实际改动及其依赖决定。

- **追踪状态及其使用方。** 对照原版与拟修改路径，查明被跳过、提前或重建的初始化写入了
  哪些状态、这些状态何时生效和重置，以及全部后续读取方。沿读取关系继续追踪到可观察结果；
  初始化或环境校验产生的值也可能参与脚本解码、参数长度、资源选择或时序，不能只按变量名判断用途。
- **核对修改后的执行语义。** 依据最终程序的实际读取逻辑，检查受影响指令的参数宽度、游标
  前进、下一条指令边界和跳转目标，再核对相关资源查找、加载、显示提交及等待／计时关系。
  文本读回、非文本指令字节一致和资源存在分别只覆盖自身检查范围；程序读取方式改变后，
  还需证明这些数据会按预期执行。
- **用能区分正确与错误行为的材料验证。** 从真实脚本或资源中选择经过受影响路径的最小片段，
  覆盖实际不同的参数形式和状态条件；预期边界与结果来自原版消费者或独立确认的格式依据。
  验证器应能识别错位读取、跳过后续显示或错误资源选择，不能只复用回填器的假设证明自身一致。
  复用已有检查；静态证据不能确定的状态或显示结果保留为未验证。
- **按入口差异安排场景验收。** 从受影响调用方选择代表场景。正常剧情、回想和读档可能采用
  不同初始化路径，涉及这些入口时分别检查。场景记录包含进入位置、触发步骤、预期画面或声音、
  对白推进及返回结果，并按实际影响覆盖图像淡入、动画、滚动等行为。具体预期以原版和消费者
  证据为准，不以标题窗口打开或首句对白正常代替受影响场景的检查。

把上述改动依据和检查结果保存在游戏任务资料中。分别记录文本读回、执行语义静态检查和实际
场景观察的覆盖范围；Generic 按[外部适配器验收](../generic/adapter-verification.md)汇入已覆盖
的正文消费者证据，其他游戏行为保留在任务验收记录中。最终程序、相关资源或验证逻辑改变后，
重新取得受影响证据，不沿用修改前的通过结论。

场景验收的执行者和操作方式遵循本任务已有授权。只允许被动启动观察时，Agent 完成静态分析，
将需要点击、加载存档或推进剧情的具体检查交给人工，并明确尚未观察的结果。本方法不扩大游戏
启动、重试或界面操作的权限；修复后的静态通过与实际场景确认分别报告。

## 6. 人工实机验证

实机验证由任务发起者指定的人工完成。检查清单至少覆盖：

- 标题、菜单、设置和存档界面；
- 从标题开始新游戏、进入首段剧情，以及本次修改的场景入口与返回路径；
- 主要角色对话、姓名框、选择项和剧情事件；
- 地图、战斗、物品、技能和插件界面；
- 中文字体、缺字、回退字体、换行、裁切和溢出；
- 本轮调查中标记为 `unresolved` 的场景。

反馈记录具体场景、触发步骤、看到的原文或译文和期望结果。Agent 根据反馈定位来源与所有者，
从最早受影响阶段继续，并重新执行受影响的 QA、WriteBack、字体检查和封包。

## 7. 交付说明

交付说明聚焦翻译结果：

- 译本或补丁目录；
- 已覆盖的游戏版本、语言和文本范围；
- Translate、Manual、QA、WriteBack 与字体处理结果；
- 人工实机已经验证的场景；
- 交给人工继续检查的具体场景和待确认翻译位置。
