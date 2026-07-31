# RPG Maker Extract 现行规格

```text
att --config CONFIG mv extract --name NAME [--builtin] \
  [--dialogue-rules FILE] [--rules FILE]

att --config CONFIG mz extract --name NAME [--builtin] [--rules FILE]
```

Extract 的执行者是 Builtin 与 Rules 两类能力，Lua 不在其中。

## 1. 运行方案

首次 Extract 必须显式选择 `--builtin`、`--rules FILE` 中至少一项。MV 的
`--dialogue-rules FILE` 只修饰 Builtin，不构成独立 owner。

项目保存最近一次成功采用的 Builtin/Rules 集合。之后省略全部提取选项时复用该集合；
显式提供任一 owner 时，本次集合精确替换旧方案，未列出的 owner 跳过执行，既有资产
原样保留。

- `rule = []` 的 Extract Rules 文件停用 Rules 并删除其资产；
- MV `rule = []` 的 dialogue rules 只清空姓名投影定义；
- 清理后没有可执行 owner 时，保存方案为空，下次无参数 Extract 会明确失败。

各 owner 的提交彼此独立：Builtin 成功而后续 Rules 失败时，Builtin 的新结果落库，
旧 Rules 快照保持。

## 2. 资产与身份

每个 Group 保存 kind、来源语境、自然顺序和一个或多个 Unit。Unit 内容形状由 RPG Maker
字段决定：

- 单字符串；
- 固定逐行数组；
- 固定逐项数组；
- 可自由断行数组。

Unit 身份是 `owner + group_location + unit_role`，排序字段不参与身份。Builtin 与
Rules 都保存可从冻结来源重新验证的写回 recipe。

## 3. Builtin 与 Rules

Builtin 覆盖矩阵和 kind 由当前代码与测试固定，主要包括数据库条目、系统字段、Map
信息、事件对话、选项、滚动文本和明确支持的插件参数。未列出的任意 note/meta、自定义
文件或脚本文本不自动提取。

Rules 的字段、来源、路径、捕获、顺序和错误范围由[规则规格](rules.md)定义。

## 4. 冲突、继承和提交

每项可写位置形成 Mutation Claim。同一物理值只容许一方声明互斥修改，祖先与后代路径
之间也不得形成覆盖歧义；任一冲突都会使当前 owner 候选失败。

owner 成功提交时：

- 身份和源语境仍相同的 Unit 继承译文与状态；
- 原文、形状、Group 语境或写回关系改变的 Unit 清除旧译文；
- 删除的 Unit 与状态一并删除；
- 新 Unit 为未翻译。

Extract 只负责提取，全程不发出模型请求。成功结果必须包含 owner、Group、Unit、冲突
摘要和来源指纹，供 Translate 与 WriteBack 重新检查。
