# Placeholder 文件现行规格

Placeholder 保护控制符、模板标记和其他不能由模型改写的片段。文件使用严格 TOML：

```toml
[[rule]]
order = 'preserve'
pattern = '\\SE\[[^]]+\]'

[[rule]]
scopes = ['event_dialogue', 'event_choices']
ids = ['Map023.json:event17:page1:dialogue42']
order = 'preserve'
pattern = '<msg>(?<text>.*?)</msg>'
```

仍交给语言判断、术语匹配和模型翻译的文本片段称为 NaturalText；被 Placeholder
替换成 ATT token 的片段称为不透明保护段。

## 文件字段与匹配

上例使用 RPG Maker scope。每项只允许：

| 字段 | 要求 |
|---|---|
| `pattern` | 必填；非空、可编译的 PCRE2 表达式 |
| `scopes` | 可省略的非空字符串数组；省略表示适用于全部 kind |
| `ids` | 可省略的非空自然 Manual ID 数组；省略表示不按 ID 限定 |
| `order` | 必填；`preserve` 或 `reorder_within_slot` |

`scopes` 与 `ids` 同时出现时取交集。每个 `ids` 值必须是当前项目的完整自然 ID；未知、
重复或空 ID 会在模型请求前使资源无效。`preserve` 要求命中片段保持相对顺序；
`reorder_within_slot` 允许同一文本槽内重排，但仍要求片段身份和数量不变，也不得跨槽。
带 `text` 的 wrapper 必须使用 `preserve`。

没有 `text` 命名捕获时，完整匹配是不透明保护段。存在 `text` 捕获时，捕获本身仍是可
翻译的 NaturalText，完整匹配中捕获前后的字节分别成为不透明 wrapper。一个规则最多有
一个命名捕获，并且只能命名为 `text`。一旦规则声明 `text`，该捕获必须参与这条规则的
每一次完整匹配；例如让另一条 alternation 分支在不捕获 `text` 的情况下也能匹配，会以
`translation.placeholder.missing_text_capture` 明确失败。规则按文件顺序执行；空匹配、
无效 UTF-8 字节范围或实际保护跨度重叠也会在 Translate 准备阶段明确失败。

## 引擎负责 scope

- MV/MZ 只接受 [RPG Maker 规则规格](../rpg-maker/rules.md#61-rpg-maker-scope)列出的
  kind，并额外保护该规格列出的引擎控制符；
- Generic 把 `scopes` 与 JSONL 的 `kind` 原值精确比较，没有内置控制符。

Generic 项目使用与 JSONL `kind` 原值精确对应的 `scopes`。从 MZ Placeholder 文件迁移规则时，
逐项复核或改写 `scopes`，并作为独立 Generic 规则随 Translate 加载；两类文件共享 TOML 语法，
scope 语义分别由各自引擎负责。

## 候选验收

ATT 把匹配片段替换成临时 ATT token，再把模型结果中的 token 恢复为原片段。token 的
字符、大小写、编号、数量、顺序和允许位置必须保持；缺失、重复、改写或跨不允许边界移动
都会拒绝该 ID。

候选验收以源文保护阶段已经建立的 binding 为权威，不要求同一正则在译文标签、标点或其他
自然语言上下文中再次命中。候选保留 token 时直接核对；候选包含原始保护片段时，只把数量与
自然顺序足以唯一归属的片段转换为对应源 token。数量多或少、重叠，或者其他无法唯一归属的
情况拒绝。`fixed` 的每个槽分别绑定，允许换序的规则也只能在同一槽内换序；wrapper 仍须保持
原配对、捕获形状和拓扑。
已经唯一归属的不透明保护段，其内部字节不再参与其他原片段的计数；相交但未被完整包含的
候选片段仍须拒绝。

预期片段绑定后，ATT 仍对完整候选应用当前 Unit 的规则和 RPG Maker 内建控制契约，但扫描
结果只用于发现源 binding 之外的新身份，不反过来要求预期片段必须在译文上下文中再次命中。
新命中的 Placeholder、未知或残缺的保留 token、缺失、重复、跨槽、非法换序和 wrapper
拓扑变化都会拒绝候选。模型响应、传播目标、Manual、受管 Lua、Current 复验和 WriteBack
使用同一套源文绑定验收。

## 完整语境与规划失败

Placeholder 准备作用于完整 TaskBlock 语境，不只作用于本轮带 ID 的 Unit。模型代表显示
带 ID 的保护后原文；Current 或已确认复用的无编号 Unit 只有在目标文本能够按该 Unit 的
Placeholder 绑定建立安全表示时才显示目标文本，否则显示保护后原文。只有带 ID 的 Unit
建立响应恢复和验收契约。

任一 Unit 的原文无法完成保护或语言投影时，整次 Translate 规划失败，不发送任何模型任务，
数据库保持不变。诊断定位到失败 Unit，完整 Group 与 TaskBlock 保留原有语境。

Placeholder 问题会区分 worker 启动、PCRE2 匹配、空匹配、缺少 `text` 捕获、无效范围、
重叠、跨文本单元边界和保留 token namespace。公开诊断只保留规则文件或当前项目规则、
自然规则号、可读条目位置、直接原因和修改方法，不输出编码位置、数据库键、内部状态或
游戏正文。规划因此失败时不发模型请求，数据库保持不变，`translation.finished` 明确记录
本次 Translate 失败。

## 保存规则与复验

`translate --placeholders FILE` 在模型请求之前完整解析并原子替换当前项目规则。省略参数
时复用当前规则；`rule = []` 显式清空。解析失败时项目保持不变，也不发出请求。

Manual check/apply 使用检查时的当前 Placeholder 验证新译文。数据库重新读取当前人工或
自动译文时也使用当前强契约；契约变化后正文不再合法，就保留正文并转入 Rejected。
