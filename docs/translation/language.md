# 语言现行规格

项目 Init 保存规范化后的源语言 ID 与目标语言 ID。两者必须不同，并且 Translate
配置的 `[[languages]]` 中必须存在对应的源语言模块。

语言 ID 按 BCP 47 常用写法规范化，例如 `JA` 变为 `ja`、`zh-hans` 变为 `zh-Hans`。
首次 Init 时由操作者明确提供游戏语言，ATT 不自动猜测。

## 源语判断

语言模块负责判断一段 NaturalText 是否需要翻译，以及译文是否仍含不允许的源语
残留。完全空白、没有可翻译源语内容或完全由 Placeholder 保护的 Unit 不请求模型，
但仍可作为同组语境。

当前配置形状取决于语言类型。日语配置例如：

```toml
[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
```

- 日语译前判断只要 NaturalText 含有平假名、片假名或支持范围内的汉字，就会请求翻译；
  因此只有汉字而没有假名的日文名称、系统文本和对白不会被排除；
- 标点、迭代符号或长音符本身不能让一段文本进入翻译，必须同时存在上述源语字符；
- `minimum_kana_characters` 是译后残留检查的正整数阈值，不是译前准入条件。它检查
  `allowed_terms` 之外连续出现的假名；目标译文中的汉字不按日语残留处理；
- `allowed_terms` 列出允许保留在目标文本中的源语片段；
- 未在当前语言类型中声明的字段严格拒绝。

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
  也可能不分配临时 ID；不要用它表达“译文中允许保留的专名或按键名”；
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

Translate 启动时先校验全部语言定义，再按项目的源语言 ID 精确选择模块；找不到
匹配定义时，会在发出任何模型请求之前失败。

## 写回符号修复

Generic、MV 和 MZ 的 WriteBack 永久使用同一套、与源语言无关的译文符号修复。原文是
符号模板：修复器只把译文中已经存在、并且能够唯一对应到原文位置的符号替换成原文实际
字符，不插入、删除或移动任何字符，也不改变译文空白。译文新增符号保留，原文存在但译文
缺失的符号不补。

候选包括全部 ASCII punctuation、Unicode `P*` 标点，以及 NFKC 后成为单个 ASCII
punctuation 的全角或兼容字符，因此全角 `＋`、`＝` 属于候选；除此之外，非 ASCII emoji、
货币符号和数学符号不参与。只有语义相同的符号族才能对应，普通 `/` 与 `\`、省略号与句点
等不同含义的符号不得互换；单词内部的撇号不按引号开闭关系处理。

Placeholder、RPG Maker 控制符和 Rules Literal 形成 opaque 段，正文匹配不读取或改写其
字节；引号和括号的结构可以跨 opaque 边界继续判断，但原文与译文的 opaque 边界结构无法
可靠对应时，整个 Unit 保留原译文。普通多解只保留相应译文字符，其他能够确定的位置仍可
修复；资源分配或修复不变量无法成立时也保留整个 Unit 并继续 WriteBack。符号修复本身不把
合格译文变成错误，也不隐藏数据库、Placeholder、候选验证或发布的真实失败。用户取消会
中止修复规划并沿用 WriteBack 的取消语义，不记为内部跳过。
