# Generic JSONL 现行规格

输入根内每个普通 `.jsonl` 文件由零行或多行 Group 组成。每条物理行恰好是一个完整 JSON
object：

```json
{"id":"scene-1","kind":"dialogue","units":[{"id":"line-1","text":"こんにちは\n世界"}]}
```

## 1. Group

Group 只允许以下字段：

| 字段 | 要求 |
|---|---|
| `id` | 非空字符串，在整个项目中唯一 |
| `kind` | 非空字符串，由外部操作者定义 |
| `units` | 至少包含一个 Unit 的数组 |

一条物理行是不可拆开的语义组。JSON 中的 `\n` 是 `text` 解码后的 LF，并不产生新的
物理 JSONL 行。

## 2. Unit

Unit 只允许：

| 字段 | 要求 |
|---|---|
| `id` | 非空字符串，在所属 Group 中唯一 |
| `text` | 字符串，是唯一会被翻译的字段 |

`text` 可以为空、只含空白或含 LF，但不得含 CR 或 NUL。一个 Unit 的源 `text` 始终对应
一个目标 `text`；模型可以自由改变其中 LF 的数量。

ID 和 kind 只拒绝空字符串。纯空白字符串按原值是合法身份；ATT 不替外部操作者推断其
含义。校验不会改变保存值，它们按原值区分大小写，不做 trim 或 Unicode 归一化。
Group ID、Unit ID 和 kind 不翻译。

## 3. 严格读取

ATT 严格拒绝：

- 重复 JSON key、未知字段或缺少字段；
- 空白物理行；
- 无效 UTF-8、无效 JSON，或 `text` 解码后含 CR 或 NUL；
- 重复 Group ID，或一个 Group 内重复 Unit ID；
- 空 ID、空 kind 或空 units。

空输入目录和空 JSONL 文件合法。输入文件可以使用 LF 或 CRLF。ATT 递归读取输入根内、
扩展名精确为小写 `.jsonl` 的普通文件；符号链接和其他文件既不是输入，也不会进入
WriteBack 输出。

自然顺序依次为规范化相对文件路径、文件内行序和 `units` 数组顺序。文件路径只决定任务
与输出位置，不是 Group 身份。

## 4. 设计边界

当前契约没有 `translate`、`context`、`role`、metadata、格式版本或扩展字段。需要更多语境时，
由外部操作者把相关文本放进同一 Group；不应向 JSONL 添加 ATT 不读取的私有字段。

[可执行示例](examples/sample.jsonl)包含不同 kind 和多行 `text`。
