# 语言现行规格

项目 Init 保存规范化后的源语言 ID 与目标语言 ID。两者必须不同，并且 Translate
配置的 `[[languages]]` 中必须存在对应的源语言模块。

语言 ID 按 BCP 47 常用写法规范化，例如 `JA` 变为 `ja`、`zh-hans` 变为 `zh-Hans`。
首次 Init 时由操作者明确提供游戏语言，ATT 不自动猜测。

## 源语判断

语言模块在译前判断 NaturalText 是否需要翻译，在译后报告配置范围内的源语残留。译后
发现属于 Review，不使译文失效。完全空白、没有可翻译源语内容或完全由 Placeholder 保护
的 Unit 不请求模型，但仍可作为同组语境。

## 日语模块

日语配置例如：

```toml
[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
```

- 日语译前判断只要 NaturalText 含有平假名、片假名或支持范围内的汉字，就会请求翻译；
  因此只有汉字而没有假名的日文名称、系统文本和对白不会被排除；
- 标点、迭代符号或长音符（包括 `ー` 与 `ｰ`）本身不能让文本进入翻译，必须同时存在上述
  源语字符；
- `minimum_kana_characters` 是译后残留检查的正整数阈值，不是译前准入条件。它检查
  `allowed_terms` 之外连续出现的假名；目标译文中的汉字不按日语残留处理；
- `allowed_terms` 列出允许保留在目标文本中的源语片段；
- 未在当前语言类型中声明的字段严格拒绝。

## 英语模块

英语配置例如：

```toml
[[languages]]
type = "english"
id = "en"
minimum_word_count = 1
minimum_letter_count = 2
ignored_terms = []
minimum_copied_word_count = 2
minimum_copied_letter_count = 4
allowed_terms = ["Page Up", "Page Down"]
```

- `minimum_word_count` 与 `minimum_letter_count` 是译前准入阈值；NaturalText 中至少有一个
  连续英文片段同时达到两个阈值时，该 Unit 才需要翻译；
- `ignored_terms` 只参与译前判断。匹配项从英文片段中排除，可能使 Unit 不请求模型，因而
  也可能不分配临时 ID；译文中允许保留的专名或按键名应使用 `allowed_terms`；
- `minimum_copied_word_count` 与 `minimum_copied_letter_count` 是译后源文复制检查阈值；
  只有译文中从本 Unit 原文复制的连续英文片段同时达到两个阈值，才报告
  `source_residual`；
- `allowed_terms` 只参与译后源文复制检查。匹配项仍参与译前判断和临时 ID 分配，但允许
  原样保留在译文中，不触发 `source_residual`；它适合专名、按键名、协议词、单个字母和
  确实必须保留的短语；
- `ignored_terms` 与 `allowed_terms` 都按 ASCII 大小写不敏感匹配，支持多词短语；以英文字母
  开头或结尾的配置项只在对应字母边界匹配，避免短词误伤更长单词；
- 两个列表都必须逐项表达已经确认的语义。不能把普通待译词或整类英文加入
  `allowed_terms` 来掩盖未翻译内容，也不能用 `ignored_terms` 规避本应进入翻译的 Unit。

## 目标书写系统检查

候选验收在 Placeholder 恢复和自然文本投影之后，另外检查项目目标语言的预期书写系统。
排除不透明保护段和源语言模块的 `allowed_terms` 后，整个 Unit 含有其他书写系统的字母，
却没有任何目标书写系统字母时，追加 `target_script_missing` Review。例如日译中返回完整
英文，即使没有日语假名，也会提示复核。数字、标点、符号、完全保护的内容和仅由允许词组成
的内容不单独触发这个检查；允许词沿用对应模块的精确或 ASCII 大小写不敏感匹配语义。

目标标签显式声明 `Latn`、`Hans`、`Hant`、`Hani`、`Jpan`、`Kore`、`Hang`、`Arab` 或 `Cyrl`
时，优先使用该书写系统。没有显式 script 时，支持 `zh`（汉字）、`ja`（汉字、平假名、片假名）、
`ko`（谚文、汉字）、`en/fr/es/vi`（拉丁字母）、`ar`（阿拉伯字母）、`ru`（西里尔字母）的区域
变体。其他主语言或显式 script 不猜测，不产生此项 Review；这不表示已经验证目标语言。

本检查使用 Unicode Script 属性，是保守的书写系统提示，不是语义语言识别。它不能区分
共用汉字的中日文本、拉丁字母语言、简繁体，也不能证明混合译文的每一段均已翻译。已有源语
残留检查独立执行，两个 Review 可以同时出现。项目日志和模型任务记录保存自然 Unit 位置
及原因，不将疑似问题正文写入项目日志。两类 Review 均不改变 Current、Rejected、任务完成
或 WriteBack 的语义，也不会自动重新请求模型。

## 生效时间与结果

Translate 启动时先校验全部语言定义，再按项目的源语言 ID 精确选择模块；找不到
匹配定义时，会在发出任何模型请求之前失败。

语言模块的译后判断只产生 Review，并由译后 QA 汇总。WriteBack 不执行语言分析；源语言残留
不会因此阻断发布。WriteBack 的可选标点修复只比较原文与译文的标点拓扑，是独立的正文
处理，不把语言 Review 改成 Rejected。
