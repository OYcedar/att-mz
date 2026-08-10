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
- 自由断行内容按数据库中的目标数组重建；
- 对话、选项和滚动文本按事件命令结构重建；
- Rules 的嵌套 JSON、捕获与 Literal 按原 grammar 重新编码。

未译或非 Current Unit 保留冻结原文。Partial 项目同样可以生成候选，结果会明确报告
保留原文的数量。

WriteBack 不修订正文，也不根据窗口宽度自动断行。自动译文和人工译文在进入当前状态前
已经通过同一个结构与 Placeholder 验收；WriteBack 只重新确认当前来源、owner、recipe 和
项目快照，并逐字物化数据库中的当前译文。语言、术语、符号风格、布局和运行时显示风险由
译后 QA 生成 Review；需要修订时按其自然 ID 使用 `manual export --ids`、check 和 apply，
再重新 WriteBack。

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

发布完成后按[全量验收指南](../guides/acceptance.md)检查全部输出差异、源语残留、布局风险、
组合项目覆盖和实际加载；WriteBack 成功本身不是整个翻译任务的完成证明。

每次命令写 `publication.started` 和唯一 `publication.finished`。成功时 RPG Maker 汇总保存
使用译文和保留原文的 Unit 数；失败时 result 为 `not_published`、
`recovery_required` 或 `outcome_unknown`。具体问题由同次可读 `diagnostic.publication`
说明，不附内部诊断引用。

恢复路径固定为 `<parent>/.directory-publish/<target-name>/{stage,backup,journal}`。保持项目、
输入、目标和这些路径不变，按[目录发布规格](../runtime/directory-publishing.md)处理诊断中的
对象、原因、影响和处理办法。发布已经生效但只剩清理失败时，修正占用或权限后重新运行同一目标
WriteBack，下一次准备会先恢复。journal 损坏、必要 backup 缺失或结果未知时禁止重跑试探，
也不手工移动或删除恢复目录。
