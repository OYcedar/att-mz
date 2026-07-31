# 术语文件现行规格

## 职责范围

ATT 从已经确定的术语要求开始，负责读取、校验、匹配、保存和应用当前项目的术语文件。
本规格不负责从游戏内容中发现候选，也不判断哪些表达值得成为术语。

ATT 术语的预期内容是需要固定写法的游戏专有名称和游戏内有明确定义的词，包括必须保留
的官方名称或固定拼写；普通名词表、词频表和全文词典不属于预期输入。需要从游戏或文本
制作、筛选或审查术语表时，先使用
[通用游戏术语表制作 Skill](../../skills/extract-game-terminology/SKILL.md)，再把已确认
条目转换成下面的严格 TOML。该 Skill 的中立工作记录不是 ATT 可直接读取的文件。

一项 ATT 术语表达一个规范原文身份及其要求采用的目标写法。可选 `triggers` 表达能够激活
同一翻译要求的原文写法，不是多个目标译法。当前术语文件没有按文件、Group、kind 或
上下文限定作用域的字段；同一 trigger 若在项目内需要互相冲突的译法，就不能用一个全局
术语条目表达。

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

字段按原值处理，不做 trim 或 Unicode 归一化。条目、术语和 trigger 必须保持唯一；
显式空 `triggers`、控制字符、未知字段和重复 TOML key 都会被拒绝。

省略 `triggers` 与显式写空数组含义不同，后者是无效输入：

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

术语只提供翻译要求：不替换源文，也不产生新的 Unit。`term = []` 是合法的显式空
术语集。

`translate --terms FILE` 在模型请求之前完整读取、严格解析并原子替换当前项目的术语。
省略参数时复用项目当前术语。文件解析失败时项目术语保持不变，也不开始模型请求。

MV、MZ 和 Generic 项目分别保存术语；即使它们属于同一个游戏，也不会自动同步。
