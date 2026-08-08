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

ATT 从冻结来源建立完整内容树，并按 recipe 把当前译文写回对应 RPG Maker 值。人工译文
优先于自动译文：

- 普通字符串替换完整值；
- 固定逐行或逐项内容保持规定的槽数和空槽；
- 自由断行内容按目标数组重新布局；
- 对话、选项和滚动文本按事件命令结构重建；
- Rules 的嵌套 JSON、捕获与 Literal 按原 grammar 重新编码。

未译或非 Current Unit 保留冻结原文。Partial 项目同样可以生成候选，结果会明确报告
保留原文的数量。

### 写回前符号修复

WriteBack 在布局和文档改写前对自动译文执行与源语言无关的全局符号修复。修复器使用原文符号
作为模板，只替换译文中能够唯一对应的现有符号；不插入、删除或移动字符，译文空白、
Placeholder、内建控制符和 Rules Literal 逐字保留。引号和括号按开闭及嵌套关系判断，
可以只修复一对符号中损坏的一端。局部多解不妨碍其他确定位置继续修复；修复器内部无法
安全完成时保留该 Unit 原译文并继续构建候选。该步骤不是 Translate 验收，也不隐藏后续
布局、候选验证或发布错误。用户取消不计为内部跳过，仍按 WriteBack 的现有取消终态结束。
发布汇总报告尝试、实际修复、内部跳过的 Unit 数和替换符号数。人工译文不经过符号修复。

对自动译文执行符号修复前，WriteBack 使用当前 Placeholder 规则和对应引擎内建控制符重新
验收。原文或自动译文无法完成 Placeholder 保护、语言投影，或两者实际绑定不一致时，
WriteBack 以可读 ID 说明对象、原因和修改方法，不发布候选，也不把该错误计入符号修复内部
跳过。人工译文已经由 Manual apply 检查，不因 Placeholder 配置后来变化而重新判为无效。

布局器无法安全自动断行时，WriteBack 保留该译文的显式硬换行并继续构建候选，不把它
伪报成已经自动布局。成功结果中的 `manual_layout_units` 与结构化人工布局诊断逐项对应。
每项诊断包含可读 ID、显示区域和本次采用的全角字符宽度，例如
`Map023.json:event17:page1:dialogue42`。公开输出不要求理解内部位置或角色编码。

布局宽度计算把 `\n<...>` 和 `\N<...>` 姓名框识别为零宽控制序列。姓名框必须包含闭合
`>`，且其中不得出现控制字符；其中的 `\n[145]` 等方括号控制语法作为姓名框内容保留，
独立出现的方括号控制语法仍按原规则识别。未闭合或含控制字符的姓名框无法证明显示边界，
因此仍返回 `Manual`；姓名框后的可见正文照常参与宽度和安全断点判断，真实超宽不会被忽略。

`Manual` 只表示布局器无法保证整个显示请求的阅读质量，不携带具体原因。它可能来自行
过宽、没有安全断点、未知控制序列、保留的 Placeholder 前缀、控制字符，或同一请求中
没有译文的原文段。处理每项诊断时，优先重新运行 Manual export，按相同可读 ID 找到当前
译文；需要完整 Group 语境时，把全部待查 ID 合并到一次
`ctx.translation.context(ids)`，再检查该显示请求内的译文、保留原文、控制序列和已有硬换行：

- 原因只是行过宽或没有安全自动断点时，按显示区域和宽度加入显式硬换行，在 TOML 中保持
  `fixed` 的数组形状或按 `free` 自然分行，再运行 check/apply；
- 无效 Placeholder、控制字符或译文语法返回相应 Translate 或修订步骤纠正；
- 控制语法对游戏有效、但布局器无法理解，或问题来自必须保留的原文时，不破坏语法来追求
  零告警；记录判断，保留该诊断，并在隔离游戏副本的全部相关场景中确认实际显示正确。

修订后重新 WriteBack。能够由显式硬换行解决的条目应不再告警；已经证明为游戏有效
但布局器无法理解的内容可以继续告警，其完成证据是逐项记录和实际加载，不是告警消失。

发布成功后，这些诊断逐项写入 stderr，并以 `diagnostic.write_back` 写入当前 RunId 的
JSONL。payload 只保存对象、原因和修改方法；汇总中的 `manual_layout_units` 提供总数。
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

每次命令写 `publication.started` 和唯一 `publication.finished`。成功时 RPG Maker 汇总保存 translated/original/
auto-wrapped units、插入换行、全角缩进、manual-layout units，以及符号修复尝试 Unit、
实际修复 Unit、内部跳过 Unit 和替换符号数；失败时 result 为 `not_published`、
`recovery_required` 或 `outcome_unknown`。具体问题由同次可读 `diagnostic.publication`
说明，不附内部诊断引用。

恢复路径固定为 `<parent>/.directory-publish/<target-name>/{stage,backup,journal}`。保持项目、
输入、目标和这些路径不变，按[目录发布规格](../runtime/directory-publishing.md)处理诊断中的
对象、原因和修改方法。发布已经生效但只剩清理失败时，修正占用或权限后重新运行同一目标
WriteBack，下一次准备会先恢复。journal 损坏、必要 backup 缺失或结果未知时禁止重跑试探，
也不手工移动或删除恢复目录。
