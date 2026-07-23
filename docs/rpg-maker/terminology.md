# RPG Maker 术语文件现行规格与制作指南

本文是 MV/MZ `--terms` 文件的唯一现行规范，同时说明如何从游戏事实制作可用术语表。
翻译阶段文档只说明资源生命周期和 state，不重复字段定义。外部作者只需依据本文即可
编写生产输入；源码不是冲突时的第二套规范。

规范代码块采用与[规则文件](rules.md#2-共同根结构严格解析与生命周期)相同的标记：
`valid` 可直接解析，`invalid` 必须拒绝，`illustrative` 只展示材料或结果。

## 1. 完整根结构和字段表

根恰好包含 `term` 数组。非空文件由一个或多个 `[[term]]` 组成：

<!-- att-example: valid -->
```toml
[[term]]
term = "蒼月団"
translation = "苍月团"

[[term]]
term = "星紋石"
translation = "星纹石"
triggers = ["星紋石", "星紋の石"]
```

权威空术语表为：

<!-- att-example: valid -->
```toml
term = []
```

| 字段 | 类型 | 必填/默认 | 约束 |
|---|---|---|---|
| `term` | string | 必填 | 非空白、无首尾空白、无任何控制字符；全文件唯一 |
| `translation` | string | 必填 | 非空白、无首尾空白、无任何控制字符 |
| `triggers` | string array | 可选；缺席时等于 `[term]` | 显式数组非空；每项非空白、无首尾空白；允许内部 LF；全文件 trigger 唯一 |

未知字段、重复字段、错误类型、零字节、只有注释、缺少 `term` 根、重复身份都作为普通
无效当前输入拒绝。当前条目只包含本规格列出的单译名与匹配字段。

显式 `triggers` **完整替换**默认 `[term]`，不是追加。如果规范 term 本身也应触发，必须
把它写进数组。

<!-- att-example: valid -->
```toml
[[term]]
term = "王都"
translation = "王都"
triggers = ["王都", "王都アルカ"]
```

<!-- att-example: invalid -->
```toml
[[term]]
term = "王都"
translation = "王都"
triggers = []
```

## 2. 字符、控制符与 Markdown 安全

文件必须是 UTF-8。所有比较都按原始 Unicode scalar 序列逐字进行：区分大小写，不 trim
命中材料，不做 NFC/NFD 等 Unicode 规范化，也不做全角/半角、假名或父语言折叠。

`term` 和 `translation` 禁止所有 Unicode control 字符，包括 LF、CR、TAB 和 NUL；它们
必须各自是一条普通展示文本。`trigger` 只允许一种 control：内部 LF (`U+000A`)；CR、
NUL、TAB 及其他 control 全部拒绝。LF 不能出现在 trigger 首尾，因为所有字段仍禁止
首尾空白。

<!-- att-example: invalid -->
```toml
[[term]]
term = "星\t港"
translation = "星港"
```

<!-- att-example: valid -->
```toml
[[term]]
term = "航海宣言"
translation = "航海宣言"
triggers = ["海へ\n出よう"]
```

术语进入模型 Prompt 时由 ATT 按 Markdown 字面文本转义。作者应填写真实字符，不要预先
加 Markdown 反斜杠；例如 term `A|B` 仍写 `term = "A|B"`，ATT 输出时会保护 `|`，使
模型看到的内容不会变成表格结构。反斜杠、反引号、强调、链接、标题、列表和引用标记
同样按字面处理。TOML 自身需要的字符串转义仍必须遵守。

<!-- att-example: valid -->
```toml
[[term]]
term = "[A|B]"
translation = "[甲|乙]"
```

## 3. trigger 如何匹配

trigger 是区分大小写的字面子串，不是正则。匹配发生在 Placeholder 投影之后，只扫描
`NaturalText`：

- `Value`：逐个 NaturalText 段扫描；内部 LF 可以由含 LF 的 trigger 命中；
- `Lines`：按每个数组元素及其中的 NaturalText 段扫描；不把相邻元素拼接；
- opaque 协议壳和 ATT token 不扫描；
- 不跨 `OpaqueBoundary`、不同 `Lines` 元素或不同语义单元拼接。

因此，插件 wrapper 属性里的专名不会误触发术语，wrapper 中真正开放的正文仍会命中。

<!-- att-example: illustrative -->
```text
术语 trigger：勇者
原文：<actor title="勇者">勇者</actor>
Placeholder：<actor title="勇者">(?<text>.*?)</actor>

前壳 title 中的“勇者” -> 不命中（opaque）
捕获正文中的“勇者”     -> 命中（NaturalText）
```

同一条术语的一个或多个 trigger 在同一语义输入中命中任意次数，该术语只输出一次。两条
不同术语都命中时，输出保持**术语文件顺序**。文件顺序不是优先级、覆盖或最长匹配规则；
它只是命中结果的稳定顺序。语义冲突的两个条目不会因先后顺序被解决。

命中的有序条目进入 Standard Prompt、Standard translation state 和 Lua
`prepared.terms`，三处复用同一次结果。重排两个已命中条目会改变有序命中集并使 state
失效；重排、添加或删除未命中的条目不会影响该单元。

## 4. 两套唯一性约束

`term` 唯一与 trigger 唯一是两套独立约束：

- 任意两个条目的 `term` 不得相同；
- 展开默认 `[term]` 后，所有 trigger 在全文件不得重复；同一条目内也不得重复；
- 不同条目可以使用相同 `translation`，因为多个源概念可以共享目标译名；
- 一个字符串作为某条的 `term`，并不自动禁止它作为另一条的 trigger；真正禁止的是
  `term` 集内部重复或 trigger 集内部重复。

<!-- att-example: valid -->
```toml
[[term]]
term = "守護者"
translation = "守护者"

[[term]]
term = "番人"
translation = "守护者"
```

<!-- att-example: invalid -->
```toml
[[term]]
term = "守護者"
translation = "守护者"
triggers = ["ガーディアン"]

[[term]]
term = "ガーディアン"
translation = "守护者"
triggers = ["ガーディアン"]
```

## 5. 文件生命周期与失败范围

`translate --terms FILE` 会在 Standard 规划前完整读取、严格解析、校验并替换项目中的
canonical 术语资源；省略 `--terms` 复用项目当前资源，不重新读取上次文件；提供
`term = []` 才清空。解析失败不会部分更新资源，也不会开始本轮 Standard 模型请求。

术语文件合法只证明格式成立，不证明译名正确。命中条目、实际 Placeholder、原文、
语言对、语言模块、公共 Prompt/Client 语义等共同进入受影响单元的 state。未命中术语
不会成为该单元依赖。

## 6. 从游戏事实建立候选

术语表约束“这个概念在其他自然正文再次出现时采用什么口径”，不是结构化字段译文的
外部镜像。优先从已经证明玩家可见的标准资产、已启用插件和运行时消费者交叉取证。

| 候选来源 | 建议一起阅读的上下文 | 常见稳定概念 |
|---|---|---|
| `Actors.name/nickname` | `profile`、该角色 Speaker 下的对白 | 人名、称号、阵营 |
| `Classes.name` | 角色、技能和说明中的用法 | 职业、体系名 |
| `Skills.name` | `description`、`message1/2` | 核心技能、术式 |
| `Items/Weapons/Armors.name` | 各自 description、剧情提及 | 关键物品、装备系列 |
| `Enemies.name` | 战斗与剧情称呼 | 关键敌人、种族 |
| `States.name` | `message1`～`message4` | 重要状态、战斗概念 |
| Map `displayName` | 地图事件、任务、对话 | 地点、组织 |
| System 类型数组 | 技能、装备与说明 | 属性和系统分类 |
| 启用插件玩家文本 | 最终窗口和解析协议 | 插件引入的概念 |

MZ Speaker 来自 `101.parameters[4]`。MV 应使用已经验证的姓名投影建立 Speaker/Body，
不能把任意第一行当姓名。完整 Builtin 字段矩阵见[提取规格](extraction.md#3-builtin-覆盖矩阵)。

候选是否收录取决于稳定性与正文价值，不取决于机械频次。只出现一次但承担关键世界观
身份的专名可以收录；大量一次性句子、编号变体、代码、路径、ID 和协议壳通常不应收录。

## 7. 字段译文不等于正文术语

字段译文回答“这个完整字段最终显示什么”，术语回答“模型在自然正文遇到这个概念时
保持什么口径”。一个字段可能同时包含括号、编号、两个专名、控制符和完整句子；它仍可
整体翻译，但不应因此整段进入术语表。

<!-- att-example: illustrative -->
```text
字段原文：【蒼月団・北支部】
字段译文：【苍月团·北支部】
更合适的正文术语：蒼月団 -> 苍月团
```

术语不是机械替换表。模型仍应根据目标语言语法、称谓和语气自然使用统一译名。

## 8. trigger 的选择与消歧

省略 `triggers` 适合规范 term 本身就是唯一稳定写法。只有同一概念确有简称、异体完整
写法或稳定文本变体时才扩充。过短通用词、单字、常见后缀和单个拉丁字母容易误命中。

当前格式没有按文件、角色、事件或场景的 scope。一个短词在不同语境需要不同译名时，
条目顺序无法解决，应选择以下之一：

1. 找到能稳定区分含义的更完整 trigger；
2. 原文本身可区分时建立不同 term；
3. 无法可靠区分时不收录，让完整上下文决定，并在结果审核中检查。

互相包含的不同 trigger 可以同时命中，既不最长者胜出，也不按文件顺序覆盖。它们的
译名必须语义兼容。

## 9. 可复制的完整正例与反例

<!-- att-example: valid -->
```toml
[[term]]
term = "ミレア"
translation = "米蕾娅"

[[term]]
term = "星読み"
translation = "观星者"
triggers = ["星読み", "星を読む者"]

[[term]]
term = "蒼月団"
translation = "苍月团"
```

<!-- att-example: invalid -->
```toml
[[term]]
term = " ミレア"
translation = "米蕾娅"
priority = 10
```

上面的反例同时违反首尾空白和未知字段；解析器不承诺先报告哪一个。

## 10. 一次写对的验证清单

1. 每个候选都能指向真实玩家消费者，而不是“看起来像自然语言”；
2. `term` 是干净概念，`translation` 能跨载体复用；
3. trigger 正例命中，常见同形词、协议壳和路径反例不命中；
4. 大小写、Unicode 组合形式和 LF 与真实源字节一致；
5. 多个同时命中的条目语义兼容，不依靠顺序覆盖；
6. 结构化字段最终译文与正文口径一致；
7. 修改已命中译名会重译，未命中资源变化不会制造无关重译；
8. Placeholder 边界正确，协议壳中的同形词不会进入术语命中。

模型请求、state 和 Current 语义见[翻译规格](translation.md)；Placeholder 的 NaturalText
边界见[规则文件](rules.md#7-terminology-与协议壳的边界)。
