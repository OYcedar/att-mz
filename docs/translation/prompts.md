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

一次请求只包含渲染后的 system message，以及当前 TaskBlock 的一条 `json` Markdown
代码块 user message：

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

ATT 使用唯一、闭合的 `json` Markdown 围栏包住稳定的两空格缩进 JSON，并把完整代码块
作为实际 user message 发送给模型；模型任务记录保存同一份正文。围栏和缩进不改变字段、
顺序或语义，TaskBlock 装箱继续按紧凑的完整原文结构投影计数，因此 Profile 的字符目标仍
不是最终 user message 的硬上限。

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

原文标点和 ASCII 符号也是翻译输入的一部分。模型优先保留原文实际字符、数量和强调关系，
不得仅因目标语言习惯默认改成全角符号或另一种引号、分隔符、括号和连接符；列表分隔符、
格式边界和协议要求的符号必须逐字保留。只有已经确认某个符号属于普通自然语言表达，且目标
语言为了正确、自然的表达确实需要调整时，才改变它。模型输出前静默检查符号；思考模式只
说明真正影响译法的判断，不要求输出空泛的“已检查符号”。

## 3. 四种响应

Assistant 正文必须只包含一个 JSON object。它可以是裸 JSON，也可以放在唯一、闭合的
`json` Markdown 围栏中；围栏外只允许 JSON 空白字符，不能有说明、其他代码块或第二个
JSON 候选。
两个开关形成四种格式。

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
映射。`think` 是操作者明确开启的强制审阅输出；ATT 即使不把它写入译文状态，也不会把它
当作可省略的附属字段。

原文回显模式中，每个 ID 的 value 必须且只能包含字符串数组 `source` 和
`translation`。ATT 校验 `source` 的字段与数组形状，但不比较其内容，也不把它写入
译文状态；译文仍只通过 ID 关联。回显是操作者明确开启的强制审阅输出，字段无效只拒绝
对应 ID，不影响其他合法 ID。

这是有意的协议边界：`source` 用来促使模型在生成译文时显式关注原文，并供任务记录的人工或
Agent 审阅，不是 ATT 的响应关联证据。ATT 不消费其内容来推断、纠正或拒绝 ID 归属；开启
`source_echo` 不能防止结构合法的语义性 ID 错配。

公共解析按原始顺序保留全部 key，包括重复 key 和非法 ID。ID 只接受 `"0"` 或不带前导
零的规范十进制字符串；负数、`"00"`、`"01"`、非数字和溢出值都无效。引擎据此识别
重复、非法、未知和缺少的 ID，再逐项检查 translation 的 strict/free 形状、Placeholder、
语言和引擎语义。

ATT 先识别上述两种合法外层。规范围栏只确定 JSON 正文范围，移除它不属于 JSON 修复，
也不建立修复记录。随后 ATT 按严格 JSON 解析选定正文；只有严格解析确认属于 JSON 语法
错误或意外结束时，才使用固定的保守修复规则重新建立严格 JSON。合法 JSON 的字段、类型、
重复字段和业务结构错误不进入修复。对于不符合正式外层但仍能唯一确定 JSON 的响应，修复
可以去除唯一的非规范围栏或前后说明；它还可以清理注释，规范单引号、裸 key、已有的裸
字符串值、控制字符和无效转义，并补正能够由当前语法状态唯一确定的引号、冒号、逗号和
结构闭合符。修复不得删除、覆盖、合并或重排字段，不得合并多个根值，也不得补造缺失的
value、ID 或其他业务字段。存在多个 JSON 候选、未结束字符串、多种合理解释，或引用字符串
仅以空白相邻而既可解释为漏逗号、也可解释为未转义正文引号时，整份响应仍然无效。

合法响应外层处理、严格解析和保守修复都无法建立 JSON、最外层 object 或思考模式根结构
无效时，该任务不提交。根结构有效时，每个 ID 独立验收；重复、非法、未知、缺少或 value
无效形成 Partial，其他合法 ID 可以保存。

## 4. Prompt 变化与既有正文

实际组成的 system message、两个 Prompt 开关、Profile 和 Client 只决定后续
模型请求如何装箱、发送和验收。改变这些选择不会改变已经接受的译文正文，
也不会自行使它失去 Current 或触发重译。

项目语言对是译文实际适用性的一部分，不是 Prompt 身份；语言对变化后，旧语言对
的正文保留但不再发布，直到对应新语言对的结果通过验收并原子替换。人工译文
同样绑定它实际填写时的语言对，但不绑定 Prompt、Profile 或 Client。
