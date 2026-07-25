# RPG Maker 系统提示词编写指南

本文面向编写或微调 RPG Maker Standard Translate system Prompt 的作者。它说明 ATT 如何
选择、校验和装配外置 Prompt，以及模型响应必须满足的机器协议。本文首先关心的不是译文
是否优美，而是响应能否被 ATT 解析、关联到正确单元并通过结构验收。

本文描述当前唯一实现。资源路径、模板变量、任务消息或响应信封发生变化时，必须同步
修改解析器、资源、测试与本文，不能在外置文案中另行发明协议。

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

这些结果属于结构化翻译结果，不一定表现为进程崩溃或非零退出码。一个任务可为
`Complete`、`Partial`，或因 `AllOutputsRejected`、`ModelResponseUnusable` 等原因不可用；
命令仍需按全部任务和 planning-unresolved 汇总为 `Complete`、`Partial` 或 `Unavailable`。

## 2. 配置、locale 与资源选择

Translate 必须完整提供：

```toml
[prompts]
root = "prompts"
locale = "auto"
thinking_output = false
```

该表只允许这三个字段；Translate 遇到缺失字段、未知字段或错误类型都会按配置输入错误
失败。Help、Version、Init、Extract 与 WriteBack 不消费或校验这些内部字段。

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
不复制资源正文；这是配置诊断的职责与可读体积边界，不表示 Prompt 或其他资源内容属于
敏感信息。

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
关闭思考输出时，最终 system message 只有渲染后的 `system.md`；开启时精确装配为：

```text
rendered system.md + "\n\n" + thinking.md
```

即只追加一份同 locale 的思考要求，并固定使用两个 LF。装配后的完整 system message 参与
translation state，并随每个 TaskBlock 完整发送；Profile 的字符装箱目标只参与最终 user
message 中完整文本组的 TaskBlock 分组。切换 locale、切换 `thinking_output`，或修改本轮
实际读取的任一资源，都会使依赖旧 Prompt 的受影响译文不再 Current，但 system message
的字符数不参与该目标计算。关闭时没有读取 `thinking.md`，因此它的变化不会影响该模式的
system message 或 state。

所有 `system.md` 都必须把裸 JSON 规定为默认响应；只有 system message 末尾实际存在
`thinking.md` 的“思考输出要求”时，才允许先输出该片段规定的内容。无论使用哪种模式，
最终 JSON 后永远不得追加内容。这个条件是母版与九份翻译版共同拥有的机器契约，不能在
微调时改成由模型自行选择是否思考。

## 4. 模型实际收到什么

每个 Standard TaskBlock 仍然只发送两条消息：

1. `system`：按上一节渲染并按模式装配的完整 system Prompt；
2. `user`：ATT Planner 自动生成的 Markdown 任务载荷。

user message 不是 JSON，也不是 ATT 的内部领域对象。它只携带模型完成本次翻译所需的
术语、语境、待翻译内容、临时 ID 和输出形状。语言对、文件路径、owner、内部 kind、
传播目标和去重原因不会重复写入 user message；翻译方向由模板渲染后的 system message
建立。

Planner 生成的标题、字段标签和形状说明统一使用英文。所有本地化 Prompt 都必须保留并
解释协议字面量 `single line`、`free line breaking`、
`N lines, corresponding line by line`、`N items, corresponding item by item`。这些固定文本
不随 Prompt locale 本地化；翻译内容本身的语言只由项目语言对建立，不能根据英文标签推断。

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
字段名也不决定输出形状：Planner 会把源 `Value` 含 LF 的 Scalar 标成
`free line breaking`，部分可自然扩展的 profile/description 即使当前只有一行也使用该
形状；模型始终只服从条目上实际给出的形状标记。

多行、逐项严格对齐及允许重排换行的内容使用 `> ` 作为 Markdown blockquote 前缀；前缀
不属于原文，只有 `> ` 的行表示空槽。`single line` 内容直接出现在冒号后。输出不得复制标题、标签、
`[ID]`、形状说明或 `> ` 前缀，只返回 ID 到译文字符串数组的映射。

## 5. 翻译要求与四种输出形状

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

所有数组元素都必须是 JSON 字符串，解码后不能包含 CR、LF 或 NUL。需要多行时必须拆成
多个数组元素；把换行写进一个字符串，即使 JSON 语法有效，也会使该 ID 被拒绝。

严格对齐的源空槽必须对应精确空字符串 `""`，不能是 `" "`；源槽非空时，对应输出
不能是空字符串或纯空白。`free line breaking` 不要求保持原行数。去除 ATT token 等受保护片段后，
候选还必须包含非空白自然语言文本；只返回 token 或其他不透明片段也会被拒绝。

## 6. 响应信封模式

ATT 把配置解析成受信的 `TranslationResponseEnvelope`，并让 Planner 与 Executor 共享
同一个值，避免 system Prompt 开关和解析器失配：

```text
TranslationResponseEnvelope
├─ JsonOnly
└─ ThinkingThenJson
```

### 6.1 `JsonOnly`

`thinking_output = false` 选择 `JsonOnly`。模型必须直接输出裸 JSON object；任何
`<why>` 思考信封都会被拒绝。响应仍可使用第 7 节说明的有限 JSON 围栏容错，但 Prompt
始终要求裸 JSON。

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
启用 Standard 任务记录时，合法 Thinking 作为非权威正文按原生 Markdown 呈现。裸 JSON
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
之前，不能出现在 `</why>` 与 JSON 之间。剥离可选思考信封后，JSON parser 还容忍唯一
一层独占首尾行的无标记、`json` 或 `JSON` Markdown 围栏。这是接收端的有限容错，不是
Prompt 可以依赖的输出格式；外置 Prompt 必须始终要求裸 JSON。

JSON 根成功后，以下问题只拒绝对应 ID：

| 输出问题 | 对应 ID 的结果 |
|---|---|
| ID 缺失 | `Missing` |
| 同一 ID 出现多次 | `Duplicate` |
| value 不是数组，或数组中含非字符串 | 形状无效 |
| `single line` 或严格对齐数组长度错误 | 行数不匹配 |
| 字符串含 CR、LF 或 NUL | 结构行无效 |
| 空槽位置错误，或非空槽只返回空白 | 空白形状不匹配 |
| ATT token 丢失、重复、损坏、未知或跨严格槽移动 | Placeholder/ATT token 验收失败 |
| 只含 ATT token、控制片段或空白，没有自然语言文本 | 自然语言文本缺失 |
| 译文字符串内部含 BOM | BOM 验收失败 |
| 同时混用 ATT token 与原始控制片段，无法唯一恢复 | Placeholder 正规化歧义 |
| 译文仍有源语言残留 | 源语残留验收失败 |

非法或未知 ID 形成协议诊断并被忽略，不能代替缺失的正确 ID。其他 ID 仍可接受，因此
任务可以得到 `Partial`；没有任何预期 ID 通过时原因为 `AllOutputsRejected`；全部通过才
是 `Complete`。这套逐 ID 规则在两种响应信封模式下完全相同。

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

严格对齐条目按对应槽分别校验 token，因此 token 不能跨行或跨项移动。允许重排换行的条目按
整个 ID 校验 token 多重集，只允许 token 在同一 ID 的输出行之间随自然表达移动，不能
跨 ID 移动。

解析器能在少数无歧义场景中把模型输出的原始控制片段正规化回 token，但这只是接收端
恢复能力，不是 Prompt 契约。Prompt 作者必须要求模型保留所见 token，不能依赖恢复逻辑。

## 9. `thinking.md` 的思考要求

仓库提供的每个本地化 `thinking.md` 都要求模型针对整个 TaskBlock 只输出一组思考信封，
并对每个带 `[ID]` 的条目实际分析：

1. 说话人、听话人、省略主语和可能人称；
2. 人物关系、语气、情绪和敬语；
3. 术语含义及目标语言自然表达；
4. 占位符、控制符、ATT token 和行结构；
5. ID、行数、源语残留和最终格式。

不能只写“已检查”或直接给结论。协议不强制固定栏目标题，ATT 也不判断分析内容是否
正确；这不降低模型应完成上述分析的 Prompt 要求。`</why>` 后直接输出 system 母版规定
的 JSON，JSON 不得放入 `<why>`，最终 JSON 后不得追加内容。

## 10. Prompt i18n 与允许的微调

`zh-Hans` 是语义母版，其余九个 locale 必须保持相同机器契约和思考要求。翻译或微调
本地化文件时，以下内容必须原样保留：

- `{{source_language}}` 与 `{{target_language}}`；
- JSON、`[ID]`、`<why>`、`</why>`、ATT token 等协议字面量；
- Planner 的英文输入标记 `single line`、`free line breaking`、
  `N lines, corresponding line by line`、`N items, corresponding item by item`；
- 两种信封的选择条件、ID/数组/空槽/token 规则以及 JSON 终止边界。

可以微调的是翻译风格、表达提示和本地语言说明；不能增加模板变量、改变资源布局、要求
额外输出字段、另包 JSON、改变标签、放宽或收紧行形状，或让 JSON parser承担提示词中
没有建立的协议。`thinking_output` 只控制人工可读的 `<why>` 输出，不控制供应商原生
reasoning/thinking 参数；这些参数仍完全属于所选 Client 的受信 `parameters`。

## 11. 最低检查清单

修改 Prompt 后，至少确认：

- 十个 locale 均各有两份非空 UTF-8 普通文件；
- 所有 `system.md` 只含且都包含两个必需变量，所有 `thinking.md` 不含变量；
- 两类文件保留相同协议字面量和 `<why>` 边界；
- system Prompt 只把带 `[ID]` 的源语言内容翻译为目标语言；
- JSON 中每个实际 ID 恰好一次，value 只能是字符串数组；
- `single line`、严格逐行、严格逐项和 `free line breaking` 分别遵守自己的形状与空槽规则；
- JSON 字符串不含 CR、LF 或 NUL，ATT token 按形状逐字保留；
- JsonOnly 直接输出 JSON，ThinkingThenJson 恰好输出一组非空 `<why>` 后接 JSON；
- 最终 JSON 后没有任何内容。

验证样本至少覆盖无术语、存在术语、无 ID 语境、`single line`、`free line breaking`、带空槽的
严格对齐、多个 ATT token，以及合法/非法思考信封。只测试一个单字符串 JSON 不能证明完整契约。

任务输入的规划事实和结果状态见[翻译现行规格](translation.md#5-任务规划与模型消息)，
ATT token 的来源与恢复规则见[规则编写指南](rules.md#6-placeholder-rules)，术语资源见
[术语表制作指南](terminology.md)，HTTP 外层响应与 assistant content 的关系见
[Chat Completions 运行根](../runtime/chat-completions.md)。普通项目日志、终端和通用
诊断保持运行级结构化摘要，不复制完整 Prompt、messages、思考正文、原文、译文或模型
正文；这是各自职责、稳定 schema 和体积边界，不是敏感性定义。开启高级记录后，这些
Standard 任务正文按可读 Markdown 写入，并精确替换其中出现的 API key 实际值，见
[Standard 翻译任务记录现行规格](task-records.md)。
