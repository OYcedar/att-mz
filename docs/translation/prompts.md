# Prompt 与模型协议现行规格

## 1. 共用资源与组合

MV、MZ 和 Generic 共用固定的中文 Prompt 资源：

```text
<att-dir>/prompts/translation/
├── system.md
├── thinking.md
├── rules/
│   ├── plain.md
│   ├── thinking.md
│   ├── source-echo.md
│   └── thinking-source-echo.md
└── examples/
    ├── plain.md
    ├── thinking.md
    ├── source-echo.md
    └── thinking-source-echo.md
```

`system.md` 必须存在、为非空 UTF-8，并且只能使用 `{{source_language}}` 和
`{{target_language}}` 两个模板变量，两者都必须出现。它只说明翻译任务和质量要求，
不提未启用的响应模式。

`[prompts].thinking_output` 和 `[prompts].source_echo` 是互不排斥的必填布尔值。ATT 按
当前组合选择一份规则和一份示例，并按以下顺序组成 system message：

```text
system.md
+ thinking.md（仅 thinking_output = true）
+ 当前组合的 rules 文件
+ 当前组合的 examples 文件
```

思考关闭时不读取 `thinking.md`。每次请求只包含当前组合的规则和一个完整示例，不向模型
介绍其他响应格式。Prompt 的指令固定为中文；项目语言对只替换源语言和目标语言变量，
UI 语言不参与资源选择。

四种响应模式的示例使用相同的输入和译文事实，只改变响应包装。完整示例必须同时展示
`free` 合并原文行与拆分原文行，以及 `strict` 保持数量和空槽位置，避免示例暗示
`free` 也要逐行对应。

## 2. User message

一次请求只包含渲染后的 system message，以及当前 TaskBlock 的一条 JSON user message：

```json
{
  "terminology": [
    {
      "source": "原术语",
      "translation": "参考译法"
    }
  ],
  "groups": [
    {
      "kind": "dialogue",
      "units": [
        {
          "role": "speaker",
          "text": ["无编号语境"]
        },
        {
          "id": "0",
          "role": "body",
          "type": "free",
          "text": ["需要翻译的原文"]
        }
      ]
    }
  ]
}
```

ATT 使用稳定的两空格缩进 JSON 把该对象作为实际 user message 发送给模型；模型任务记录
保存同一份正文。缩进不改变字段、顺序或语义，TaskBlock 装箱继续按紧凑的完整原文结构投影
计数，因此 Profile 的字符目标仍不是最终 user message 的硬上限。

- 没有实际术语时省略 `terminology`；术语是专名和既有译法的参考，不是脱离语义的机械
  替换命令。
- `groups` 和 `units` 保留 TaskBlock 内的完整自然顺序。`kind` 表示实际 Group 类型；
  `role` 只在数据源确有角色含义时出现。
- 只有本轮需要模型输出的 Unit 才有 `id` 和 `type`。语境 Unit 省略这两个字段，不使用
  占位编号。
- `id` 是字符串，在每条 user message 中从 `"0"` 开始连续编号并保持唯一；下一个
  TaskBlock 重新从 `"0"` 开始。
- `text` 始终是字符串数组。`strict` 要求译文数组数量相同并逐项对应，包括空字符串所在
  位置；`free` 允许按目标语言自然重新分行，但仍须返回至少一个字符串。

消息只携带完成本次翻译所需的语境、实际术语、角色、形状和临时 ID，不发送项目数据库
身份。Generic 的 text 按 LF 拆成数组并保留空行和末尾空槽；译文验收后再用 LF 连接。

## 3. 四种响应

响应必须是一个裸 JSON object，不能带 Markdown 围栏或前后说明。两个开关形成四种
格式。

思考关闭、原文回显关闭：

```json
{"0":["译文"]}
```

思考开启、原文回显关闭：

```json
{"think":"具体翻译判断","translations":{"0":["译文"]}}
```

思考关闭、原文回显开启：

```json
{"0":{"source":["原文"],"translation":["译文"]}}
```

思考与原文回显同时开启：

```json
{"think":"具体翻译判断","translations":{"0":{"source":["原文"],"translation":["译文"]}}}
```

思考模式的根对象必须且只能包含非空字符串 `think` 和 object `translations`。思考缺失、
空白、类型错误、重复或存在其他根字段时，整份响应无效。非思考模式的根对象直接是 ID
映射。

原文回显模式中，每个 ID 的 value 必须且只能包含字符串数组 `source` 和
`translation`。ATT 校验 `source` 的字段与数组形状，但不比较其内容，也不把它写入
译文状态；译文仍只通过 ID 关联。回显字段无效只拒绝对应 ID，不影响其他合法 ID。

公共解析按原始顺序保留全部 key，包括重复 key 和非法 ID。ID 只接受 `"0"` 或不带前导
零的规范十进制字符串；负数、`"00"`、`"01"`、非数字和溢出值都无效。引擎据此识别
重复、非法、未知和缺少的 ID，再逐项检查 translation 的 strict/free 形状、Placeholder、
语言和引擎语义。

ATT 先按严格 JSON 解析完整 Assistant 正文。只有严格解析确认属于 JSON 语法错误或意外
结束时，才使用固定的保守修复规则重新建立严格 JSON；合法 JSON 的字段、类型、重复字段
和业务结构错误不进入修复。修复可以去除唯一的 Markdown 围栏或前后说明，清理注释，
规范单引号、裸 key、已有的裸字符串值、控制字符和无效转义，并补正能够由当前语法状态
唯一确定的引号、冒号、逗号和结构闭合符。修复不得删除、覆盖、合并或重排字段，不得合并
多个根值，也不得补造缺失的 value、ID 或其他业务字段。存在多个 JSON 候选、未结束字符串
或多种合理解释时，整份响应仍然无效。

严格解析和保守修复都无法建立 JSON、最外层 object 或思考模式根结构无效时，该任务不提交。根结构有效时，
每个 ID 独立验收；重复、非法、未知、缺少或 value 无效形成 Partial，其他合法 ID 可以保存。

## 4. Prompt 变化

Translate 把实际组成的 system message、两个 Prompt 开关、语言对和模型 Client 的语义
身份纳入自动译文状态。任一相关内容改变时，受影响的自动译文不再是 Current。人工 Lua
修订不绑定 Prompt 或 Client。
