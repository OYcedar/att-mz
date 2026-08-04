# 语言现行规格

项目 Init 保存规范化后的源语言 ID 与目标语言 ID。两者必须不同，并且 Translate
配置的 `[[languages]]` 中必须存在对应的源语言模块。

语言 ID 按 BCP 47 常用写法规范化，例如 `JA` 变为 `ja`、`zh-hans` 变为 `zh-Hans`。
首次 Init 时由操作者明确提供游戏语言，ATT 不自动猜测。

## 源语判断

语言模块负责判断一段 NaturalText 是否需要翻译，以及译文是否仍含不允许的源语
残留。完全空白、没有可翻译源语内容或完全由 Placeholder 保护的 Unit 不请求模型，
但仍可作为同组语境。

当前配置形状取决于语言类型。例如：

```toml
[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
quote_repair_pairs = [["“", "”"], ["‘", "’"]]
```

- 日语译前判断只要 NaturalText 含有平假名、片假名或支持范围内的汉字，就会请求翻译；
  因此只有汉字而没有假名的日文名称、系统文本和对白不会被排除；
- 标点、迭代符号或长音符本身不能让一段文本进入翻译，必须同时存在上述源语字符；
- `minimum_kana_characters` 是译后残留检查的正整数阈值，不是译前准入条件。它检查
  `allowed_terms` 之外连续出现的假名；目标译文中的汉字不按日语残留处理；
- `allowed_terms` 列出允许保留在目标文本中的源语片段；
- `quote_repair_pairs` 是成对的单字符开引号与闭引号，供 WriteBack 在写入候选前按源文
  引号拓扑规范化译文；Translate 不因引号样式或开闭方向差异拒绝合格译文；
- 未在当前语言类型中声明的字段严格拒绝。

Translate 启动时先校验全部语言定义，再按项目的源语言 ID 精确选择模块；找不到
匹配定义时，会在发出任何模型请求之前失败。

WriteBack 只读取当前项目源语言对应的 `id`、`type` 和 `quote_repair_pairs`。日文配置存在
候选引号对时，WriteBack 会在 Generic、MV 和 MZ 的候选构建阶段共用同一规范化器；无法
唯一确认源文拓扑、译文数量或布局时保持译文原样，不阻断发布。
