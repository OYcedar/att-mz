# RPG Maker Translate 现行规格

```text
att mv translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML]

att mz translate --name NAME [PROFILE_ID] \
  [--terms TERMINOLOGY_TOML] [--placeholders PLACEHOLDER_TOML]
```

Translate 不使用 Lua。Profile 来自公共 `[translation].profiles`；省略时复用项目最近
一次成功保存的 Profile。术语和 Placeholder 分别保存在当前 MV/MZ 项目。

## 1. 准备与 Current

ATT 从项目数据库读取 Extract 资产和冻结来源指纹，应用语言模块、实际术语命中、
RPG Maker Placeholder 与内置控制符，为每个 Unit 判定去向：Current、需要模型、可以
复用，或不能处理。

自动译文状态绑定当前原文、完整 Group 语境、语言、实际术语、实际 Placeholder、Prompt
和 Client 语义。目标译文正文与语义状态分开保存，因此独立 Lua 修改已有译文后仍可
保持 Current；模型提交的 CAS 仍同时比较旧译文和状态。

## 2. 全局去重

去重在整个当前项目内执行，以翻译角色、完整原文、保护后的输入和实际 Placeholder
绑定确定同族成员。

- 没有 Current：自然顺序最早的未译成员请求模型，再向未译成员传播；
- 只有一种 Current：向未译成员复用，不请求模型；
- 已有多种不同 Current：全部保留，不报冲突；有未译成员时，从未译成员重新选代表；
- 已有 Current 永远不被覆盖。

去重族、代表项和传播关系只在本次 Translate 运行中计算，不写入数据库。相同原文
需要不同译文时，在自动翻译后使用[原子数据库 Lua](../lua/README.md)精确修改。

## 3. TaskBlock 与模型形状

RPG Maker 的语义范围、Group 顺序和 Profile 目标字符数共同决定 TaskBlock。Group 保持
完整、绝不拆开；单个 Group 超过目标字符数时就独占一个任务。

发送 Group 时保留全部相关 Unit 和说话人语境，只有模型代表获得临时数字 ID。模型
value 始终是字符串数组：

- `single line`：恰好一个无 LF 字符串；
- `N lines, corresponding line by line`：恰好 N 项并保持空槽；
- `N items, corresponding item by item`：恰好 N 项并保持空槽；
- `free line breaking`：至少一项，可以自然改变数组项数量。

数组中每个字符串不得含 CR、LF 或 NUL。公共 Prompt 与信封见
[Prompt 规格](../translation/prompts.md)。

## 4. 验收、并发和结果

每个 ID 独立检查 JSON 形状、数组结构、Placeholder、源语残留，并执行语言模块能够
证明安全的修复。译文语言以项目语言对和 Prompt 的明确要求为准，ATT 不做短文本语言
识别式的猜测。合法 ID 可以提交，其他 ID 形成 Partial；整个响应无法解析时，该任务
不提交。

任务之间可以并发执行，确认和提交仍按自然顺序进行。已确认的前序进度落库后，后续
失败或取消都不会把它带走。提交时重新检查来源、owner、Unit、译文和语义状态，发现
并发变化则不覆盖新状态。

结果分为 Complete、Partial 与 Unavailable。退出码成功只说明结果已经明确；Partial
和 Unavailable 仍意味着完整翻译目标尚未达成。
