# Prompt 与模型协议现行规格

## 1. 资源选择

每个引擎分别读取：

```text
<prompts.root>/<prompt-engine>/<locale>/system.md
<prompts.root>/<prompt-engine>/<locale>/thinking.md
```

`prompt-engine` 为 `rpg_maker` 或 `generic`。MV 与 MZ 共用 `rpg_maker`，Generic 使用
`generic`。

`[prompts].locale` 为具体 locale 时精确选择；`auto` 使用目标语言能够映射到的 locale，
ATT 不回退到其他目录。`system.md` 必须存在、为非空 UTF-8，并且只能使用
`{{source_language}}` 和 `{{target_language}}` 两个模板变量，两者必须都出现。

`thinking_output = false` 时只读取 `system.md`。为 `true` 时还读取非空 UTF-8
`thinking.md`，将其附加到 system message；`thinking.md` 不接受模板变量。

## 2. 模型消息

一次请求只包含：

1. 渲染后的 system message；
2. 当前 TaskBlock 的一条 user message。

user message 只携带模型完成当前任务所需的语境、实际术语、形状标记和临时数字 ID，
不发送项目数据库身份。

MV/MZ 的 value 是字符串数组，数组形状由 RPG Maker 翻译规格决定。Generic 的 value
是一个字符串，并允许在 JSON 字符串中使用 `\n` 表示 LF：

```json
{"1":"你好\n世界","2":"爱丽丝"}
```

## 3. 响应信封

默认响应必须是裸 JSON object，不能有 Markdown 围栏或前后说明。启用 thinking 输出时，
响应必须为：

```text
<why>非空思考</why>
{"1":"译文"}
```

只允许一组精确小写、无属性的 `<why>...</why>`；其后除空白外只能是最终 JSON。

公共解析严格检查 thinking 信封和 JSON object，并按原始顺序保留全部 key，包括重复 key
与不能解释为规范十进制数字的 key。引擎据此识别重复、非法、未知和缺少的 ID，再检查
value 形状、Placeholder、语言和自身结构。

信封、JSON 语法或最外层 object 无法解析时，该任务不提交。信封与 object 有效时逐项
处理。每个 ID 独立验收；重复、非法、未知、缺少或 value 无效的 ID 形成 Partial，
其他合法 ID 可以保存。

## 4. Prompt 变化

Translate 把实际 `system.md`、可选 `thinking.md`、语言对和模型 Client 的语义身份纳入
自动译文状态。相关内容改变时，受影响的自动译文不再是 Current。人工 Lua 修订不绑定
Prompt 或 Client。
