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

- `minimum_kana_characters` 是正整数；
- `allowed_terms` 列出允许保留在目标文本中的源语片段；
- `quote_repair_pairs` 是成对的单字符开引号与闭引号；
- 未在当前语言类型中声明的字段严格拒绝。

Translate 启动时先校验全部语言定义，再按项目的源语言 ID 精确选择模块；找不到
匹配定义时，会在发出任何模型请求之前失败。
