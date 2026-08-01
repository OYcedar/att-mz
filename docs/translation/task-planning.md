# TaskBlock 规划现行规格

MV、MZ 和 Generic 共用同一套 TaskBlock 装箱、临时 ID、JSON 消息和公共响应规则。
引擎仍各自负责 Current、语言、Placeholder、术语、去重、引擎特有验收和提交。

## 1. Extract 先建立文本层次

Translate 接收的文本必须先由 Extract 整理成以下稳定层次：

```text
Semantic Scope
└─ Group
   └─ Unit
```

- Unit 是最小翻译和验收对象，保留自己的身份、角色、原文与来源语境；
- Group 是不能拆开的语义整体，包含理解其中任一 Unit 所需的全部兄弟 Unit；
- Semantic Scope 是允许连续组合 Group 的最大自然范围，TaskBlock 不能跨越它。

Generic 中一个 JSONL 文件就是一个 Semantic Scope，一行是一个 Group。RPG Maker 的
Semantic Scope、Group 和 Unit 由 Extract 保存的物理顺序和引擎语义明确建立，Translate
不得再通过相邻路径或 owner 类型猜测层次。

## 2. 固定规划顺序

Translate 始终按以下顺序工作：

```text
完整 Scope / Group / Unit
→ 按完整原文的稳定表示装箱
→ 判断每个 Unit 是模型代表还是语境
→ 在每个完整块内分配临时 ID
→ 过滤没有任何 ID 的完整块
→ 渲染并发送其余完整块
```

装箱时不读取译文状态。Current、复用、全局去重、非源语、完全保护、Placeholder token、
术语命中、临时 ID 数值和以前的任务记录都不能改变块边界。相同 Extract 数据和相同
Profile 目标必须反复得到完全相同的 Scope、Group、Unit 范围；重试只允许改变哪些 Unit
带 ID，以及无编号 Unit 显示原文还是目标译文。

## 3. 稳定字符目标

`target_task_user_message_characters` 是稳定源文投影的装箱目标，不是最终 user message 的
硬上限。稳定投影只使用 Group 类型、Unit 角色、完整原文和固定 JSON 消息结构；它不使用
实际临时 ID，也不根据本轮是否需要模型输出来增删 Unit。

Group 不拆分，TaskBlock 不跨 Semantic Scope。当前块加入下一个完整 Group 后超过目标时，
就在该 Group 前结束当前块；单个 Group 自身超过目标时独占一块。术语、目标译文、
Placeholder token 和数字 ID 会让最终消息长度发生变化，但不会触发再次拆箱、合并或回填。

整个语料可以为空；Semantic Scope 和 Group 一旦存在就必须非空。数量不一致、字符计数或
临时 ID 溢出属于明确的规划错误，不能截断、饱和计算或猜测对应关系。

## 4. 临时 ID 与无 ID 块

每个完整块按 Unit 自然顺序分配临时 ID。只有本轮模型代表获得 ID；编号在每个含 ID 的块
内从 `0` 连续开始。其余 Unit 在 JSON user message 中省略 `id` 和 `type`，只提供语境，
不要求模型输出。

完整块没有任何 ID 时，本轮不发送它。过滤只是只读视图：不能把过滤后相邻的块重新合并，
也不能把其中的 Group 移到其他块。TaskBlock 不持久化；每次 Translate 都从权威 Extract
数据重新得到相同的完整规划。

## 5. 完整语境、术语与 Placeholder

每个完整 Group 的所有 Unit 都执行 Placeholder 保护、NaturalText 投影、语言判断和术语
准备，包括没有 ID 的 Unit 和完全没有 ID 的 Group。一个实际发送的 TaskBlock 必须按原顺序
包含其完整 Group 范围内的全部 Unit。

- 模型代表显示带 ID 的保护后原文；
- Current 或已确认复用的语境显示通过该 Unit Placeholder 绑定建立的安全目标文本；
- 没有有效目标文本的语境显示保护后原文；
- 只有带 ID 的 Unit 建立输出、Placeholder 恢复和响应验收契约；
- TaskBlock 合并其中全部 Group 的术语命中，并按术语文件顺序提供一次。

任一 Unit 的原文无法完成 Placeholder 或语言投影时，不能删除它后发送残缺 TaskBlock。
包含它的块不发送，并由相应引擎按现行规划失败语义报告；其他完整块是否继续执行由该引擎
已有的部分结果规则决定。

## 6. Partial 重试

假设一个完整块依次包含 A、B、C、D，第一次只有 A、C、D 通过验收。下一次 Translate
仍使用同一个完整块：A、C、D 省略 ID 并以安全目标译文提供语境，B 是唯一带 `"0"` 的
Unit。Task Record 只记录实际发出的块，但保存的 user message 必须包含这个完整
TaskBlock 的全部语境。
