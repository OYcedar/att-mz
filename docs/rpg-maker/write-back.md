# RPG Maker WriteBack 现行规格

```text
att mv write-back --name NAME
att mz write-back --name NAME
```

WriteBack 不使用 Lua。它只读取冻结来源、当前提取资产和译文，在项目工作区生成：

```text
<att-dir>/projects/<mv|mz>/<name>/write_back/
```

原游戏与冻结来源在整个过程中保持原样。

## 1. 候选构建

ATT 从冻结来源建立完整内容树，并按 recipe 把 Current 译文写回对应 RPG Maker 值：

- 普通字符串替换完整值；
- 固定逐行或逐项内容保持规定的槽数和空槽；
- 自由断行内容按目标数组重新布局；
- 对话、选项和滚动文本按事件命令结构重建；
- Rules 的嵌套 JSON、捕获与 Literal 按原 grammar 重新编码。

未译或非 Current Unit 保留冻结原文。Partial 项目同样可以生成候选，结果会明确报告
保留原文的数量。

### 写回前文本规范化

WriteBack 在布局和文档改写前按项目源语言读取 `[[languages]]` 中的写回规范化配置。
日文 `quote_repair_pairs` 只用于按源文已经确认的引号拓扑修复译文中的混合或整体写反
引号；源文不完整、译文数量不一致、拓扑改变或无法唯一判断时保留原译文。该步骤属于
候选构建，不是 Translate 验收，不会因为引号样式差异阻断合格译文。

布局器无法安全自动断行时，WriteBack 保留该译文的显式硬换行并继续构建候选，不把它
伪报成已经自动布局。成功结果中的 `manual_layout_units` 与结构化人工布局诊断逐项对应。
每项诊断都包含：

- 受影响逻辑单元的精确 `group_location`；
- 每个逻辑单元的 `role`；
- 显示区域 `region`；
- 本次判断采用的 `max_fullwidth_chars`。

布局宽度计算把 `\n<...>` 和 `\N<...>` 姓名框识别为零宽控制序列。姓名框必须包含闭合
`>`，且其中不得出现控制字符；其中的 `\n[145]` 等方括号控制语法作为姓名框内容保留，
独立出现的方括号控制语法仍按原规则识别。未闭合或含控制字符的姓名框无法证明显示边界，
因此仍返回 `Manual`；姓名框后的可见正文照常参与宽度和安全断点判断，真实超宽不会被忽略。

`Manual` 只表示布局器无法保证整个显示请求的阅读质量，不携带具体原因。它可能来自行
过宽、没有安全断点、未知控制序列、保留的 Placeholder 前缀、控制字符，或同一请求中
没有译文的原文段。处理每项诊断时，按 `group_location + role` 使用
[Lua](../lua/README.md)取得唯一 locator、当前译文和完整 Group 形状，再检查该显示请求内
的译文、保留原文、控制序列和已有硬换行：

- 原因只是行过宽或没有安全自动断点时，按 `region` 与 `max_fullwidth_chars` 加入显式硬
  换行，用 `ctx.translation.set` 保持原来的字符串或字符串数组形状并提交；
- 无效 Placeholder、控制字符或译文语法返回相应 Translate 或修订步骤纠正；
- 控制语法对游戏有效、但布局器无法理解，或问题来自必须保留的原文时，不破坏语法来追求
  零告警；记录判断，保留该诊断，并在隔离游戏副本的全部相关场景中确认实际显示正确。

修订后重新 WriteBack。能够由显式硬换行解决的 locator 应不再告警；已经证明为游戏有效
但布局器无法理解的内容可以继续告警，其完成证据是逐项记录和实际加载，不是告警消失。

发布成功后，这些诊断逐项写入 stderr，并以 `diagnostic.write_back` Warn occurrence 写入
当前 RunId 的 JSONL。每个 occurrence 的具体 RpgMaker issue 保存 `group_location`、`role`、
`region` 和 `max_fullwidth_chars`，`resolution` 固定为 `adjust_manual_layout`；只有
`publication.finished` 中的 `manual_layout_units` 总数不足以让操作者找到需要处理的位置。
人工布局本身不改变成功发布的业务结果；若警告无法呈现，按独立的进程呈现失败处理。

## 2. 完整验证

发布前重新检查：

- 来源、owner 与 Mutation Claim 指纹；
- 每个 recipe 的原值、目标位置和结构；
- 修改范围没有重复、祖先/后代或跨 owner 冲突；
- 全部 JSON、事件命令、数组形状和控制符有效；
- 未声明位置与冻结来源逐字一致。

候选必须重新解析，并证明改动只落在受管范围内。WriteBack 只搬运文本：不执行脚本，
也没有发布后回调。

## 3. 一次发布

全部候选完成并验证后，ATT 一次替换整个 `write_back/`。任何失败都会让上一次成功
输出原样保留，半成品不会被当作成功结果。发布结果无法确认时按
[目录发布规格](../runtime/directory-publishing.md)保留现场并报告恢复位置。

`write_back/` 是一棵内容树，而不是可以直接运行的完整游戏包。把它按实际部署方式
放进隔离游戏副本、确认游戏真正读取了这些文件，这一步由操作者完成。

发布完成后按[全量验收指南](../guides/acceptance.md)检查全部输出差异、源语残留、人工布局、
组合项目覆盖和实际加载；WriteBack 成功本身不是整个翻译任务的完成证明。

每次命令写 `publication.started` 和唯一 `publication.finished`。成功时
`payload.result.kind = "published"`，其 RPG Maker 汇总保存 translated/original/
auto-wrapped units、插入换行、全角缩进和 manual-layout units；失败时 result 为
`not_published`、`recovery_required` 或 `outcome_unknown`，并引用同一
`diagnostic.publication` occurrence。

恢复判断不能只看 `run.finished`。从 `publication.finished` 取得 occurrence ID，再读取该
原子诊断的 `report.effect`、`primary` 和递归 `related`。目录发布 issue 直接保存
`output_root`、`candidate_root`、`residual_path` 或 `recovery_artifacts`；嵌套 backend
diagnostic 保存具体文件系统 code、operation、I/O kind 与 OS code。发布已经生效但收尾
失败时，effect 为 `applied_finalization_failed`，运行终态也可能是 `failed`。保持项目、
输入、目标和恢复产物不变，先按
[目录发布规格第 4 节](../runtime/directory-publishing.md#4-一次发布与恢复)排除
`filesystem.journal_corrupt`、目标与已知旧目录均缺失、缺少必要 backup 等不能自动修复的
情况，并修正实际文件系统原因。只有符合自动恢复条件时，才执行一次同一项目、同一目标的
WriteBack；下一次同目标发布准备会先按 journal 恢复。`outcome_unknown` 禁止重跑试探，
两者不能互相替代。
