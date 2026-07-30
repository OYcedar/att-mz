# 术语文件现行规格

## 文件格式

术语文件使用严格 TOML，只允许一个 `term` 数组：

<!-- att-example: valid -->
```toml
[[term]]
term = "ミレア"
translation = "米蕾娅"

[[term]]
term = "星読み"
translation = "观星者"
triggers = ["星読み", "星を読む者"]
```

每项只允许：

| 字段 | 要求 |
|---|---|
| `term` | 非空术语原文 |
| `translation` | 非空目标语写法 |
| `triggers` | 可省略的非空字符串数组；省略时等于 `[term]` |

字段不做 trim 或 Unicode 归一化。条目、术语和 trigger 必须保持唯一；显式空
`triggers`、控制字符、未知字段和重复 TOML key 都拒绝。

例如，显式空 `triggers` 不是省略该字段，而是无效输入：

<!-- att-example: invalid -->
```toml
[[term]]
term = "ミレア"
translation = "米蕾娅"
triggers = []
```

## 命中与使用

ATT 在 Placeholder 处理后的 NaturalText 中执行字面 trigger 匹配。重叠命中优先采用
更长的 trigger；仍相同时采用文件中较早的条目。

Generic 以完整 Group 的全部源文计算实际命中。一个术语即使只出现在兄弟 Unit 中，也会
进入该 Group 每个自动译文的语义状态；Group 发送给模型时，只附带该 Group 实际命中的
术语。TaskBlock 的拆分方式不会改变术语状态。

术语只提供翻译要求，不替换源文，也不会产生新的 Unit。`term = []` 是合法的显式空
术语集。

`translate --terms FILE` 在模型请求之前完整读取、严格解析并原子替换当前项目的术语。
省略参数时复用项目当前术语。文件失败不会改变项目术语，也不会开始模型请求。

MV、MZ 和 Generic 项目分别保存术语；即使它们属于同一个游戏，也不会自动同步。
