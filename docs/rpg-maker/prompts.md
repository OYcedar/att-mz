# RPG Maker Prompt 资源与模型协议现行规格及编写指南

本文面向编写或微调 RPG Maker Standard/Managed Translate system Prompt 的作者。它说明 ATT 如何
选择、校验和装配外置 Prompt，以及模型响应必须满足的机器协议。本文首先关心的不是译文
是否优美，而是响应能否被 ATT 解析、关联到正确单元并通过结构验收。

本文规定 Prompt 资源的选择与文件格式、模板装配、模型消息内容、响应信封、JSON wire
（发送和接收时使用的精确 JSON 格式）、临时 ID、输出形状和 ATT token 模型协议，同时
给出 Prompt 文件的编写与验证方法。`[prompts]` 的字段、类型和路径解析规则由
[配置现行规格](../runtime/configuration.md#4-prompt语言与-profile)规定。资源路径、
模板变量、任务消息或响应协议发生变化时，必须同步修改解析器、资源、测试与本文，不能
在翻译规格、Skill 或外置文案中另行定义一份协议。任务如何规划，模型响应的解析结果
如何用于译文检查、state、checkpoint 与最终结果，由[翻译现行规格](translation.md)规定。

## 1. 先区分四种问题

| 问题层级 | 典型问题 | ATT 结果 |
|---|---|---|
| Prompt 资源或模板 | 所选文件缺失、不是普通文件、不是 UTF-8、全为空白或模板非法 | 首次 LLM 请求前明确失败 |
| 响应信封或 JSON | 思考标签非法、JSON 前后有附文、顶层不是对象或 JSON 语法错误 | 当前 TaskBlock 为 `ModelResponseUnusable`，所有预期 ID 都未完成 |
| 单个 ID 的 wire 或候选结构 | ID 缺失或重复、值不是字符串数组、行数、空槽或 ATT token 错误 | 只拒绝受影响的 ID；其他合法 ID 仍可验收 |
| 翻译质量 | 措辞生硬、语气判断不佳、术语选择不理想 | 可能仍能通过机器验收，但译文质量较差 |

因此，“必须输出 JSON”“每个 ID 恰好一次”“按标记形状返回”和“逐字保留 ATT token”
不是改善翻译质量的建议，而是协议要求。思考模式要求模型实际分析指定事项，但 ATT 只
机械验证思考信封和非空内容，不判断分析质量是否正确。

这些协议结果不一定表现为进程崩溃或非零退出码。信封或 JSON 根非法时当前 TaskBlock
形成 `ModelResponseUnusable`；JSON 根合法后，协议允许合法 ID 与被拒绝 ID 并存。它们
如何与后续译文检查、planning-unresolved 和 checkpoint 汇总为任务及命令结果，由
[翻译现行规格](translation.md#6-模型协议之后的译文检查)规定。

## 2. 配置、locale 与资源选择

本节从配置模块已经校验的 `root`、`locale` 和 `thinking_output` 开始，说明这些值如何
选择 Prompt 文件。字段是否必填、允许的类型、相对路径从哪里解析以及哪些命令读取它们，
见[配置现行规格](../runtime/configuration.md#4-prompt语言与-profile)。

`locale` 只选择提示词说明所使用的语言，不是游戏源语言或目标语言。它接受精确小写
`auto`，或能按现有 UI i18n 规则映射到受支持语言的有效 BCP-47 locale；例如 `fr-CA`
规范为 `fr`，`zh-TW` 规范为 `zh-Hant`。资源目录只使用以下十个规范标签：

```text
ar  zh-Hans  zh-Hant  en  fr  ru  es  ja  ko  vi
```

显式 locale 覆盖 UI locale。`auto` 直接复用本进程已经按 `--ui-language`、
`ATT_UI_LANGUAGE`、Windows 用户语言和英语兜底解析出的有效 UI locale，不会再次读取
命令行、环境或操作系统。解析得到的规范 locale 决定唯一路径：

```text
<prompts.root>/rpg_maker/<locale>/system.md
<prompts.root>/rpg_maker/<locale>/thinking.md
```

仓库提供上述十个 locale 目录，每个目录各有 `system.md` 与 `thinking.md`。每次 Translate
都会重新读取本轮真正选择的资源，不做长期缓存；文件本身就是使用者微调 Prompt 的入口。

`system.md` 始终读取。`thinking_output = false` 时不读取也不校验 `thinking.md`；开启时才
读取同一 locale 的 `thinking.md`。所选文件必须存在、是普通文件、为 UTF-8，且去除首尾
空白后非空。资源只按所选 locale 的上述精确路径读取。未选择 locale 的损坏资源，以及
关闭模式下损坏的 `thinking.md`，不影响本轮运行。

资源错误在首次 LLM 请求前失败。用户诊断报告规范 locale、组件名、路径和统一检查方向，
不复制资源正文；这是配置诊断的职责与可读体积边界，不构成敏感性分类。敏感信息边界由
[Chat Completions 规格](../runtime/chat-completions.md#6-敏感信息闭集唯一权威)
唯一规定。

## 3. `system.md` 模板与装配

`zh-Hans/system.md` 是语义母版；其他九个 locale 是同一契约的本地化版本。system 模板
只支持两个变量：

```text
{{source_language}}
{{target_language}}
```

两者都必须至少出现一次，可以出现多次。ATT 用项目 metadata 中已经规范化的源、目标
`LanguageId` 逐处替换它们，不维护或插入自然语言名称表。以下任一情况都使资源无效：

- 缺少任一必需变量；
- 出现未知变量、拼写错误或 malformed 模板；
- 替换后仍残留任何 `{{...}}`。

`thinking.md` 不是模板，不能包含模板变量。两个文件都先执行 Unicode 首尾空白去除。
外置资源先建立 Standard 与 Managed 共同的翻译方向、质量要求和响应格式，但最终
system message 分成两份。Standard（以及显式读取该 Prompt 的低级 Lua）在关闭思考输出
时精确等于渲染后的 `system.md`，开启时精确装配为：

```text
rendered system.md + "\n\n" + thinking.md
```

Managed 在渲染后的 `system.md` 后固定追加由 Managed 模块提供的英文协议片段；该
片段不是 locale 资源，也不是用户配置。关闭和开启思考输出时分别精确装配为：

```text
rendered system.md + "\n\n" + managed protocol fragment
rendered system.md + "\n\n" + managed protocol fragment + "\n\n" + thinking.md
```

因此只有 Managed 请求获得该片段，Standard 消息字节与低级 Lua 的 `system_prompt` 不因
Managed shape 扩展而变化。思考要求始终位于最终 system message 末尾。装配后的对应
system message 随每个 TaskBlock 完整发送；Profile 的字符装箱目标只计算最终 user
message，不计算 system message。实际发送的完整 Prompt 如何进入 state，以及资源变化
何时使译文不再 Current，由[翻译现行规格](translation.md#7-translation-state-与-current)
规定。关闭模式没有读取 `thinking.md`，因此其内容不属于该模式装配出的 system message。

所有 `system.md` 都必须把裸 JSON 规定为默认响应；只有 system message 末尾实际存在
`thinking.md` 的“思考输出要求”时，才允许先输出该片段规定的内容。无论使用哪种模式，
最终 JSON 后永远不得追加内容。这个条件是母版与九份翻译版共同拥有的机器契约，不能在
微调时改成由模型自行选择是否思考。

## 4. 模型实际收到什么

每个 Standard 或 Managed TaskBlock 都只发送两条消息：

1. `system`：按上一节渲染并按模式装配的完整 system Prompt；
2. `user`：ATT Planner 自动生成的 Markdown 任务内容。

user message 不是 JSON，也不是 ATT 的内部数据对象。它只包含模型完成本次翻译所需的
术语、语境、待翻译内容、临时 ID 和输出形状。语言对、文件路径、owner、传播目标和
去重原因不会重复写入 user message；Managed 的 collection name、unit key、metadata
与 state 同样不发送。翻译方向由模板渲染后的 system message 建立。

Planner 生成的标题、字段标签和形状说明统一使用英文。模型输入共有五种固定标记：
`single line`、`free line breaking`、`N lines, corresponding line by line`、
`N items, corresponding item by item` 与 Managed 专用的 `single string, LF allowed`。
前四种由外置本地化 Prompt 解释；第五种由宿主追加的 Managed 协议片段解释。固定文本
本身不随 Prompt locale 本地化；翻译内容的语言只由项目语言对建立，不能根据英文标签推断。

一个任务通常形如：

```markdown
Terminology:

- 星港 → 星港

## Dialogue

Speaker:米蕾雅

Body [1] (free line breaking):

> 第一行⟦ATT_ICON_WHOLE_0000⟧
> 第二行

## Choices

Choices [2] (3 items, corresponding item by item):

> 返回
>
> 前进
```

只有带 `[ID]` 的内容需要输出。术语、分组标题以及没有 ID 的说话人、名称和其他字段只
提供语境，不产生 JSON key。无 ID 语境可能是源文，也可能是 ATT 已有或复用的目标语言
译文，因此不能把 user message 中所有自然语言都当成待翻译源文。

ID 只在当前 TaskBlock 中有效，从 `1` 连续编号；下一个 TaskBlock 会重新从 `1` 开始。
字段标签不是封闭枚举，模型应以“是否带 `[ID]`”和括号中的形状标记判断输出责任。
字段名也不决定输出形状：Standard Planner 会把源 `Value` 含 LF 的 Scalar 标成
`free line breaking`，部分可自然扩展的 profile/description 即使当前只有一行也使用该
形状；Managed `reflow` 始终标成 `single string, LF allowed`。模型只服从条目上实际给出
的标记，不能根据字段名或业务 shape 名猜测。

多行、逐项严格对齐及允许重排换行的内容使用 `> ` 作为 Markdown blockquote 前缀；前缀
不属于原文，只有 `> ` 的行表示空槽。`single line` 内容直接出现在冒号后。输出不得复制标题、标签、
`[ID]`、形状说明或 `> ` 前缀，只返回 ID 到译文字符串数组的映射。

## 5. 翻译要求与五种输入标记

模型应结合整个 TaskBlock 的术语和语境，判断主谓、省略主语、可能人称、人物关系、
语气、情绪及敬语，在忠实保留含义、风格和语域的同时使用自然目标语言。这些要求主要
影响质量；源语残留、结构和 token 另有机器验收。

每个带 ID 的条目都会直接标明形状，输出数组必须以输入标记为准：

| 输入标记 | JSON 数组要求 | 额外硬限制 |
|---|---|---|
| `single line` | 恰好一个字符串 | 源槽非空时，译文不能是空串或纯空白 |
| `N lines, corresponding line by line` | 恰好 `N` 个字符串 | 与源行逐槽对应，并保持空槽位置 |
| `N items, corresponding item by item` | 恰好 `N` 个字符串 | 与源项逐槽对应，并保持空槽位置 |
| `free line breaking` | 可以按目标语言自然表达重新断行 | 整个数组至少有一个非空白字符串 |
| `single string, LF allowed` | 恰好一个非空白字符串 | 解码后允许 LF；JSON 文本中用 `\n` 表示；禁止 CR 与 NUL |

所有数组元素都必须是 JSON 字符串。除 `single string, LF allowed` 的 LF 例外外，解码后
不能包含 CR、LF 或 NUL；`free line breaking` 的多行内容必须拆成多个数组元素。Managed
专用标记反而必须保持单元素，并把 LF 编码在该 JSON 字符串内部；拆成多个元素会拒绝该 ID。

严格对齐的源空槽必须对应精确空字符串 `""`，不能是 `" "`；源槽非空时，对应输出
不能是空字符串或纯空白。`free line breaking` 不要求保持原行数。去除 ATT token 等受保护片段后，
候选还必须包含非空白自然语言文本；只返回 token 或其他不透明片段也会被拒绝。

## 6. 响应信封模式

ATT 校验配置后得到 `TranslationResponseEnvelope`，并让 Planner 与 Executor 共享
同一个值，避免 system Prompt 开关和解析器失配：

```text
TranslationResponseEnvelope
├─ JsonOnly
└─ ThinkingThenJson
```

### 6.1 `JsonOnly`

`thinking_output = false` 选择 `JsonOnly`。模型必须直接输出裸 JSON object；任何
`<why>` 思考信封、Markdown 围栏、前置说明或后记都会被拒绝。

### 6.2 `ThinkingThenJson`

`thinking_output = true` 选择 `ThinkingThenJson`。完整 assistant content 必须是：

```text
<why>非空任意内容</why>
JSON
```

具体边界如下：

- 整个 TaskBlock 恰好一组 `<why>...</why>`；标签必须是精确小写且没有属性；
- `<why>` 必须是第一个非空白内容，不能有标题、解释或其他前置文字；
- 内容经 Unicode `trim()` 后必须非空；
- 拒绝缺失、空、未闭合、嵌套、重复、大小写变体或带属性的标签；
- `</why>` 与 JSON 之间只允许空白；JSON 不能放在 `<why>` 内；
- JSON 之后除解析器允许的首尾空白外永远不得追加总结、解释或其他内容。

ATT 验证信封后把剥离的 JSON 交给唯一的现有 JSON parser 和逐 ID 验收。它不判断分析
是否正确，也不会把思考正文放入权威结果、数据库、state、普通项目日志、终端或诊断；
启用翻译任务记录时，合法 Thinking 作为非权威正文按原生 Markdown 呈现。裸 JSON
在该模式下同样非法。

信封错误与 JSON 根错误都形成 `ModelResponseUnusable`，不会伪装成网络错误，也不会触发
只为网络故障配置的重试。信封解析不会从自然语言中猜测、截取或修复 JSON。

## 7. 最终 JSON wire 与逐 ID 验收

ATT 解析的是 Chat Completions assistant `message.content` 中剥离信封后的部分，不是
供应商 HTTP 响应的外层 JSON。形状固定为：

```json
{
  "1": ["第一段译文", "第二段译文"],
  "2": ["第一项译文", "", "第三项译文"]
}
```

硬性要求如下：

1. 顶层必须是 JSON object，不能是数组，也不能再包一层 `translations`；
2. key 必须是本次输入中出现的十进制正整数 ID 字符串；
3. ID 不能带正号、空格或前导零，例如 `"+1"`、`" 1"`、`"01"` 都非法；
4. 每个预期 ID 必须恰好出现一次，不能缺失、重复或增加未知 ID；
5. 每个 value 必须是字符串数组，不能是字符串、数字、对象或 `null`；
6. JSON 必须符合严格语法，不能使用注释、尾逗号、单引号或多个连续 JSON 值；
7. 最终 JSON 后不能再输出任何内容。

响应整体会容忍首尾空白和最开头的单个 BOM；这个 BOM 必须位于裸 JSON 或 `<why>`
之前，不能出现在 `</why>` 与 JSON 之间。剥离可选思考信封后必须直接得到 JSON；
Markdown 围栏不是合法 wire。

JSON 根成功后，以下协议或结构问题只拒绝对应 ID：

| 输出问题 | 对应 ID 的结果 |
|---|---|
| ID 缺失 | `Missing` |
| 同一 ID 出现多次 | `Duplicate` |
| value 不是数组，或数组中含非字符串 | 形状无效 |
| `single line` 或严格对齐数组长度错误 | 行数不匹配 |
| 字符串含 CR、NUL，或非 `single string, LF allowed` 标记的字符串含 LF | 结构行无效 |
| 空槽位置错误，或非空槽只返回空白 | 空白形状不匹配 |
| ATT token 丢失、重复、损坏、未知或跨严格槽移动 | Placeholder/ATT token 验收失败 |
| 译文字符串内部含 BOM | BOM 验收失败 |

非法或未知 ID 形成协议诊断并被忽略，不能代替缺失的正确 ID。某个 ID 通过上述格式检查
后，ATT 还会检查自然语言内容、源语残留和 Placeholder 恢复；这些检查的精确行为和最终
任务结果由[翻译现行规格](translation.md#6-模型协议之后的译文检查)规定。这套逐 ID
协议规则在两种响应信封模式下完全相同。

## 8. ATT token 是机器保护标记

user message 中可能出现：

```text
⟦ATT_ICON_WHOLE_0000⟧
⟦ATT_NAME_BEGIN_0001⟧
⟦ATT_NAME_END_0002⟧
```

它们代表 ATT 暂时保护、验收后再恢复的原始片段。模型必须：

- 逐字保留每个 token 的大小写、字符、编号和完整边界；
- 不删除、复制、改写、拆开、创造或翻译 token；
- 不输出输入中不存在的未知或残缺 `⟦ATT_...` 内容。

严格对齐条目按对应槽分别校验 token，因此 token 不能跨行或跨项移动。`free line breaking`
允许 token 在同一 ID 的数组元素之间移动；`single string, LF allowed` 允许 token 在同一
ID 字符串的 LF 分段之间移动。两者都按整个 ID 校验 token 多重集，绝不能跨 ID 移动。

Prompt 作者必须要求模型保留所见 token，不能依赖接收端恢复。ATT 对候选执行的
Placeholder 正规化与歧义拒绝属于译文检查，见
[翻译现行规格](translation.md#6-模型协议之后的译文检查)。

## 9. `thinking.md` 的思考要求

仓库提供的每个本地化 `thinking.md` 都要求模型针对整个 TaskBlock 只输出一组思考信封，
并对每个带 `[ID]` 的条目实际分析：

1. 说话人、听话人、省略主语和可能人称；
2. 人物关系、语气、情绪和敬语；
3. 术语含义及目标语言自然表达；
4. 占位符、控制符、ATT token 和实际 wire 标记规定的行结构；Managed 专用标记还要核对
   单元素形状、LF 位置与 token 所在 LF 分段；
5. ID、行数、源语残留和最终格式。

不能只写“已检查”或直接给结论。协议不强制固定栏目标题，ATT 也不判断分析内容是否
正确；这不降低模型应完成上述分析的 Prompt 要求。`</why>` 后直接输出 system 母版规定
的 JSON，JSON 不得放入 `<why>`，最终 JSON 后不得追加内容。

## 10. Prompt i18n 与允许的微调

`zh-Hans` 是语义母版，其余九个 locale 必须保持相同机器契约和思考要求。翻译或微调
本地化文件时，以下内容必须原样保留：

- `{{source_language}}` 与 `{{target_language}}`；
- JSON、`[ID]`、`<why>`、`</why>`、ATT token 等协议字面量；
- locale 资源负责的英文输入标记 `single line`、`free line breaking`、
  `N lines, corresponding line by line`、`N items, corresponding item by item`；
- 两种信封的选择条件、ID/数组/空槽/token 规则以及 JSON 终止边界。

可以微调的是翻译风格、表达提示和本地语言说明；不能增加模板变量、改变资源布局、要求
额外输出字段、另包 JSON、改变标签、放宽或收紧行形状，或让 JSON parser承担提示词中
没有建立的协议。`thinking_output` 只控制人工可读的 `<why>` 输出，不控制供应商原生
reasoning/thinking 参数；这些参数仍完全属于所选 Client 已经校验的 `parameters`。

## 11. 最低检查清单

修改 Prompt 后，至少确认：

- 十个 locale 均各有两份非空 UTF-8 普通文件；
- 所有 `system.md` 只含且都包含两个必需变量，所有 `thinking.md` 不含变量；
- 两类文件保留相同协议字面量和 `<why>` 边界；
- system Prompt 只把带 `[ID]` 的源语言内容翻译为目标语言；
- JSON 中每个实际 ID 恰好一次，value 只能是字符串数组；
- 五种 wire 标记分别遵守自己的数组、字符与空槽规则；
- JSON 字符串不含 CR 或 NUL；只有 `single string, LF allowed` 解码后可以含 LF，ATT token
  按对应标记逐字保留；
- JsonOnly 直接输出 JSON，ThinkingThenJson 恰好输出一组非空 `<why>` 后接 JSON；
- 最终 JSON 后没有任何内容。

验证样本至少覆盖无术语、存在术语、无 ID 语境、五种 wire 标记、带空槽的严格对齐、
Managed 单元素含 LF 与错误多元素、多个 ATT token，以及合法/非法思考信封。只测试一个
单字符串 JSON 不能证明完整契约。

任务输入的规划事实和结果状态见[翻译现行规格](translation.md#5-任务规划与模型消息)，
ATT token 的来源与恢复规则见[规则编写指南](rules.md#6-placeholder-rules)，术语资源见
[术语表制作指南](terminology.md)，HTTP 外层响应与 assistant content 的关系见
[Chat Completions 规格](../runtime/chat-completions.md)。普通项目日志、终端和通用诊断
只保存本轮运行的结构化摘要，不复制完整 Prompt、messages、思考正文、原文、译文或模型
正文。这样可以保持各类输出职责清楚、schema 稳定且大小可控，但不会增加新的敏感信息
类别。敏感信息边界与替换规则由
[Chat Completions 规格](../runtime/chat-completions.md#6-敏感信息闭集唯一权威)规定；
任务记录的呈现方式见
[翻译任务记录现行规格](task-records.md)。
