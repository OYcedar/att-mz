---
name: translate-with-att
description: 使用已打包 ATT 完成 RPG Maker MV、MZ、Generic 或组合项目的游戏翻译，从可见文本调查、Extract、术语、Translate、QA、WriteBack 一直到中文字体与封包，并根据人工实玩反馈继续返修；VX Ace、Ren'Py、Kirikiri、Bakin、TyranoScript、Unity、RealLive、Escude 等引擎经 Generic 接入时同样从本技能进入。
---

# 使用 ATT 完成游戏翻译

交付可供人工实机验证的译本，按以下主线推进：

`调查 → Extract → 术语 → Translate → QA → WriteBack → 字体/封包`

ATT 负责确定性提取、状态、模型任务、译文验收和写回；Agent 负责调查文本来源、确定所有者、
制作术语、审校译文和处理游戏特有内容。命令、格式和状态以实际 `att.exe` 同目录的现行文档为准。

术语阶段必须先由随包 Formic 从原文初筛术语候选；最终筛选、术语校验、定译、术语表核对和
译文语义审校由当前负责翻译的 Agent 直接完成。用户明确指定 Formic 的其他用途时，按其具体
要求执行。

## 使用当前发行

1. 确认 `att.exe`、发行根、游戏原版、目标目录、源语言、目标语言和翻译范围。
2. 读取发行根的 `README.md`、`docs/README.md`、
   `docs/guides/translation-project.md`，再读取当前引擎和阶段的规格。
3. 继续已有项目时，以 ATT 数据库、当前输入、日志和已有译文为续跑依据。
4. 原版游戏作为只读基线；Extract、字体应用、合并和封包使用 ATT 项目目录或隔离副本。
5. 尚未制作术语表时，开始阶段就按术语 Skill 执行 Formic 连接自检；未通过时立即报告，
   使用者可以在调查与 Extract 期间修正配置。

随包 Python 工具在 Python 3.11 或更新环境中运行。没有 Python 时，按相应指南完成调查和对账，
并在交付中说明实际采用的方法。
自行编写转换器时，通过随包 `att_toolbox.translation_export.read_translation_document` 读取
完整译文导出；项目语言首行与 Unit 记录分别处理，不把首行当作正文。

## 引擎与专题经验

ATT 原生处理 RPG Maker MV、MZ 和 Generic JSONL；其他引擎通过 Generic 接入。确认引擎或遇到
下列情况时读取对应材料，它们补充调查与验证方法，不表示 ATT 原生支持该引擎：

| 情况 | 读取 | 补充内容 |
| --- | --- | --- |
| MV 出现混合插件参数、内联姓名控制码、组合写回或大量 QA 候选 | [大型、插件密集的 MV 经验](references/game-type-large-plugin-heavy-mv.md) | 对应问题的处理方法 |
| RPG Maker VX Ace／RGSS3 通过 Generic 接入 | [VX Ace 汉化技能](../rpg-maker-vx-ace-localization/SKILL.md) | Marshal、脚本、字体与消息排版 |
| Ren'Py 通过 Generic JSONL 翻译 | [Ren'Py 汉化技能](../renpy-localization/SKILL.md) | 原生翻译模板、硬编码界面、反向转换、字体与独立补丁 |
| Kirikiri / KAG / TJS 通过 Generic 接入，或标题与菜单漏翻、对象重名、控制符混排 | [Kirikiri 显示消费者与补丁加载经验](references/engine-kirikiri-kag-tjs.md) | 显示与内部身份、实际加载入口、动态表达式及游戏侧回填 |
| RPG Developer Bakin 经外部适配器接入，或数值空白、字体模糊与布局越界 | [Bakin 原生资源往返与播放器消费者](references/engine-bakin-native-roundtrip.md) | 二进制目录、命名变量、窗口标题、原生菜单、字体绑定和布局字段 |
| TyranoScript 场景与 Electron `app.asar` 容器 | [TyranoScript／Electron ASAR 经验](references/engine-tyrano-electron-asar.md) | 显示消费者、Generic 映射、标签拓扑、字体、流式回填和独立补丁 |
| Unity / Naninovel 通过 Generic 接入，或资源已译而自定义标题、状态、弹窗仍显示原文 | [Unity / Naninovel 显示消费者与漏翻返修](references/engine-unity-naninovel-consumers.md) | 脚本与序列化界面、托管字段赋值及静态缓存、有效资源和原生调用的验证边界 |
| RealLive（`Seen.txt`、SEEN 脚本与 `Gameexe.ini`）通过 Generic 接入 | [RealLive 原生文字与姓名回填](references/engine-reallive-native-text.md) | 姓名控制与显示值、编码与原生界面、脚本返修验证 |
| 已确认 Escude 封包与字库布局 | [Escude 封包查找、字库与原生界面](references/engine-escude-native-text.md) | 封包索引与实际查找、字库和界面、验收记录 |
| 需要解包重建或修改游戏运行程序、初始化、脚本解释器或资源加载逻辑 | [运行程序改动的影响分析与验收](../../docs/guides/acceptance.md#51-运行程序改动的影响分析与验收) | 改动前追踪状态依赖，改动后按受影响入口与场景取证，遵守已有操作授权 |

普通汉化保留现有视觉设计；只有用户主动明确要求美化时，才进入 VX Ace 技能的界面现代化流程。
阶段性专题在对应章节给出：命令名称消费者见调查，Unity TMP 字体见字体与封包，动态消息见实玩反馈返修。

## 1. 调查

建立声明范围内的可见非图片文本清单，记录每类文本的来源、游戏消费者、上下文、写回位置和
唯一所有者。图片文字交给图像翻译流程，资源路径、内部键、控制符和协议外壳保留其技术含义。

调查正文时，按[排版语义与最终显示验收](../../docs/guides/acceptance.md#41-排版语义与最终显示验收)
辨认空白和换行的用途，确定任务的排版约定、可重排输入与最终排版责任。

RPG Maker MV/MZ 项目优先使用随包 `rpg_maker_survey.py` 调查标准数据、事件、活动插件参数、插件源码
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
内部键后，按 [Generic 分组规则](../../docs/generic/jsonl.md#3-从源格式建立-group-与文件范围)
沿实际剧情核对完整场景和连续对话，特别检查旁白、问答、选项与前后铺垫是否被提取工具切散。
写回位置分别保留，先修正破坏剧情关系的分组，再执行 Extract。

暂时缺少消费者证据的位置标记为 `unresolved`，并列入人工实机检查清单。用户提供的截图、场景和
触发步骤可以用于补充消费者证据和定位遗漏来源。

活动插件追加、替换标题、菜单、设置或战斗指令，或从备注协议生成命令时，按
[命令名称的真实消费者](references/rpg-maker-menu-consumers.md)核对显示槽与技术字段。
变量或插件参数传入显示调用的文字同样需要所有者；已登记译文的完成率不能证明这些位置已经进入 Extract。

## 2. Extract

按引擎规格执行 Init 和 Extract：MV/MZ 使用本轮确定的 Builtin、Extract Rules 及适用的 MV
对话姓名规则；Generic 读取项目绑定的 JSONL 输入。

MV/MZ 导出 ownership，使用 Survey 的项目继续运行 audit，核对每个位置的唯一所有者。
独立 Generic 核对 JSONL 与外部来源、自然顺序和写回位置的映射，不执行 RPG Maker ownership
或 Survey 命令。
抽查相同台词在不同场景和不同轮次的来源，保留每次出现的位置；按
[相同语境复用规则](../../docs/generic/translation.md#2-相同语境复用)核对实际模型任务，不能在
外部转换器中先按原文去重，再把一份译文填回所有位置。
外部适配器采用[外部适配器验收](../../docs/generic/adapter-verification.md)的方法与证据契约。
无修改往返之外，还要覆盖替换分支；不能通过先删除姓名标签或引用外壳掩盖消费者差异。大批量
翻译前，按 [Generic 项目](../../docs/guides/translation-project.md#32-generic-项目)用少量真实替换
文本验证原生读取路径。

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
```

这份 Manual 是本轮术语和翻译的完整语料。来源、Rules、所有权或 Extract 发生变化时，重新导出
Manual，并用新语料更新术语和后续 QA。

需要检查 RPG Maker Placeholder 候选时，在 Translate 前运行随包
`translation_preflight.py`，把确认的保护规则交给 ATT。

## 3. 术语

按[游戏术语表制作 Skill](../extract-game-terminology/SKILL.md)从完整 Manual 制作
`terminology.toml`，再交给 ATT `translate --terms`。Formic 初筛无法完成时按该 Skill 暂停并
报告，不跳过术语阶段直接进入 Translate。

## 4. Translate

使用当前 ATT 配置、术语和 Placeholder 运行对应引擎的 Translate。命令结束后按 Translate 规格
确认 NoWork、Complete 或 Incomplete 的汇总，以及失败或取消时已经保留的进度，再导出当前译文：

```powershell
att <mv或mz或generic> translation export --name <项目名> <工作目录>\translations.jsonl
```

Incomplete 中的 Partial、Unavailable 和未开始 Task 按恢复指南处理。当前 Rejected 只有显式
`--retry-rejected` 才重新请求；也可以用 `manual export --selection rejected` 导出修订。
少量剩余、语境歧义和已经定位的质量问题使用 Manual TOML 集中补译或修订。

## 5. QA

按[验收指南](../../docs/guides/acceptance.md#3-检查译文质量)运行随包 `translation_qa.py scan`，
并按其中顺序读取确定状态、未验证项和 Review 组。首次扫描就提供本次 Translate 使用的术语表；
WriteBack 和运行观察完成后，带上相应证据重新扫描，仍缺少的证据保留为未验证。

当前 Agent 直接对照原文、译文和游戏上下文，审校原意、目标语自然度、人物语气、叙事和专名
译法；缺少语境时补查相关来源或消费者。常规任务不另行启动 Formic 全量译文审校、多轮整库
检索等付费外部模型作业。

源语与混入文字提示来自 ATT 当前完整导出。优先审核相邻错位、内容串入和异常扩写组中的
原译对照；选择 Review 组生成 Manual ID 时会带上相邻语境。模型响应多出编号但要求的译文
已经全部接受时，结合任务记录 `Locations` 定位整段审校，普通续跑不会修正已经入库的误译。
不要以自由分行数量变化直接判断串句，也不按启发式相似度自动移动译文。

把确认的问题与语义审校发现合并，按自然 ID 导出到 Manual，集中修订、apply、重新导出并复查。
报告分别说明静态检查、Agent 语义审校和仍需人工实机观察的场景。

## 6. WriteBack

QA 修订完成后执行对应引擎的 WriteBack。已经确认具体位置和显示宽度时使用排版规则；规则文件
按 `docs/translation/write-back-layout-rules.md` 编写。

ATT 的断行与续行补空白遵循[现行规格](../../docs/translation/write-back-layout-rules.md#4-断行与补空白)。
按调查确定的最终排版分工和[验收方法](../../docs/guides/acceptance.md#41-排版语义与最终显示验收)
先核对代表样本，再批量处理；复查游戏最终显示行的对齐、段落、断句和宽高。

RPG Maker 输出部署到隔离游戏副本。Generic 输出交给本任务已经确定的外部反向转换，并核对每个
JSONL Unit 与实际来源位置。组合项目按真实加载顺序合并，确保每个位置采用唯一译文。

## 7. 字体与封包

RPG Maker 游戏按[字体工具指南](../../docs/guides/nwjs-font-tools.md#2-递归字体调查替换与恢复)
在隔离副本中执行 `manage_rpg_maker_fonts.py apply`；需要修改前比较或只读调查时先用 `inspect`。
可选字体包括随 Skill 提供的 Noto Sans CJK SC、Noto Serif CJK SC 和霞鹜文楷 GB。
其他引擎按实际字体加载方式处理。根据完整译文字符集检查 glyph 覆盖，并保留游戏使用的字体
名称与加载关系。

Unity 项目出现 TMP 中文方框、需要选择静态字体补丁或运行时字体插件时，读取
[Unity TMP 字体修复与 MOD 交付经验](references/engine-unity-tmp-font-fallback.md)。

封包时汇总 WriteBack、Generic 外部结果、字体和游戏原有资源，生成独立交付目录。
独立 Generic 适配器在原生读取检查前用 `AdapterVerification` 固定文件基线，记录实际源文、
最终正文、消费者检查及布局测量，再把报告通过 QA 的 `--adapter-report` 汇入。缺少真实测量
时保留布局未验证，修订封包、字体或验证程序后重新取证；完整契约见外部适配器验收。

RPG Maker MV/MZ 隔离副本用 `inspect_nwjs_runtime.py smoke` 完成启动验收，并记录实际绘制、
英文残留、像素越界和运行时错误；工具无法连接或需要记录正常游玩时，按
[NW.js 实际界面观察](../../docs/guides/nwjs-font-tools.md#1-nwjs-实际界面观察)处理。报告可以
通过 QA 的 `--runtime-report` 汇入，绑定条件见[验收指南](../../docs/guides/acceptance.md#3-检查译文质量)。

交付目录完成结构解析、可见文本残留、字体覆盖和启动检查后，按
[人工实机验证清单](../../docs/guides/acceptance.md#6-人工实机验证)交给任务发起者指定的人工验证。

## 实玩反馈返修

收到截图、原文、场景和触发步骤后，先定位真实来源和所有者，再从最早受影响的阶段继续：

- 新来源进入调查、所有权和 Extract；
- 术语变化更新术语表并审校受影响译文；
- 误译进入 Manual、QA、WriteBack 与重新封包；
- 排版问题按[验收方法](../../docs/guides/acceptance.md#41-排版语义与最终显示验收)回到实际负责的
  阶段，区分正文修订与显示处理，再复查受影响的 QA、WriteBack 和封包结果；
- 字体问题进入字体引用、glyph 覆盖和相关场景复查。

补丁后出现图像、动画、音频缺失或剧情控制异常时，回到游戏侧适配及运行程序改动，按
[影响分析与验收方法](../../docs/guides/acceptance.md#51-运行程序改动的影响分析与验收)核对受影响
路径与状态的全部消费者，并更新静态证据与人工场景检查项。

RPG Maker 动态消息的源字段已译、画面仍为原文，或全新存档也复现时，读取
[动态消息的最终显示消费者](references/rpg-maker-rendered-messages.md)，核对运行时组合、
姓名转义、插件覆写及最终绘字输入，区分结构验证、实际调用证据和人工画面确认。

每轮交付说明译本目录、覆盖范围、静态 QA 结果、人工实机检查项和仍待确认的翻译位置。

## 状态恢复

命令失败、Incomplete、取消或状态不明时，读取
`docs/guides/diagnosis-and-recovery.md`，根据当前项目状态选择恢复动作。恢复沿用现有项目、输入、
术语、Manual 和 WriteBack 结果，使已经确认的翻译继续成为后续工作的基础。
