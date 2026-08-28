# RPG Maker WriteBack 现行规格

```text
att mv write-back --name NAME [--layout-rules FILE]
att mz write-back --name NAME [--layout-rules FILE]
```

WriteBack 不使用 Lua，也不构造模型 Client、读取 Prompt/Profile 或发出模型请求。它只读取
冻结来源、当前提取资产和译文，在项目工作区生成：

```text
<att-dir>/projects/<mv|mz>/<name>/write_back/
```

原游戏与冻结来源在整个过程中保持原样。

## 1. 候选构建

ATT 从冻结来源建立完整内容树，并按 recipe 把当前译文写回对应 RPG Maker 值。人工译文
优先于自动译文。自动正文的 V2 状态必须与当前源文、完整 Group 来源语境、项目语言对、
位置、角色和写回结构精确匹配，写回前再独立执行当前 Placeholder 和结构强校验；两项都
成立才会写回。Client、Profile、Prompt、术语和语言检查阈值不参与 V2 状态判断。源语言残留仍只是一项
Review，不会拒绝候选或阻止 WriteBack。

正文处理顺序为：可选自动译文标点修复、规则命中的自动排版、独立续行补空白，再按 recipe
物化。两个正文开关及正式默认见[配置规格](../runtime/configuration.md#4-writeback-正文开关)；
排版文件、选择器、持久化和错误语义见
[WriteBack 排版规则规格](../translation/write-back-layout-rules.md)。

上述处理和最终物化都使用源文已经建立的 Placeholder binding 保护候选，不要求同一规则在
译文标签或自然语言上下文再次命中。完整候选的规则扫描仍检查新增自定义 Placeholder 和
当前位置适用的内建控制；数量、顺序、固定槽、wrapper 或保留 token 不合法时不生成候选。

- 关闭标点修复时，标点逐字采用数据库译文；开启时也只替换已经存在且唯一对应的自动译文
  标点，不增删字符或复制原文空白；人工译文不做标点修复；
- 补空白与规则断行相互独立；即使没有排版规则，也会处理译文中已经存在的硬续行；
- 未被排版规则命中的位置保持排版前文本；找不到安全断点时整项保持排版前文本；
- 规则命中的事件对话正文通过增加 `401` 物理命令物化；滚动文字通过增加 `405` 物理命令
  物化；完整单字符串标量字段只在字符串内部插入 LF；
- Speaker、Choice、固定空槽和组合字符串字段不能排版，规则命中会在发布前失败。

最终 recipe 物化仍遵守：

- 普通字符串替换完整值；
- 固定逐行或逐项内容保持规定的槽数和空槽；
- 自由断行内容按数据库中的目标数组重建；
- 对话、选项和滚动文本按事件命令结构重建；选项只改写 `102.parameters[0]`，同层 `402`
  的整数分支值及全部其他数据原样保留，不按 102 的选项数量重解释；
- Rules 的嵌套 JSON、捕获与 Literal 按原 grammar 重新编码。

未译或非 Current Unit 保留冻结原文。不匹配当前语言对或 Group 来源语境的正文和状态仍保留
在项目中，不发布，也不在 WriteBack 时删除；绑定事实恢复后，原 V2 状态可以重新匹配。
Partial 项目同样可以生成候选，结果会明确报告保留
原文的数量。

WriteBack 不使用 Init 中的统一宽度，也不扫描全部文本猜测可换行位置。语言、术语、措辞和
未被规则处理的运行时显示风险仍由译后 QA 生成 Review；需要修订时按自然 ID 使用
`manual export --ids`、check 和 apply，再重新 WriteBack。

## 2. 完整验证

发布前重新检查：

- 来源、owner 与 Mutation Claim 指纹；
- 每个 recipe 的原值、目标位置和结构；
- 修改范围没有重复、祖先/后代或跨 owner 冲突；
- 全部 JSON、事件命令、数组形状和控制符有效；
- 未声明位置与冻结来源逐字一致。

候选必须重新解析，并证明改动只落在受管范围内。新增 `401/405` 时事件列表作为完整结构
重建，并按每个输出行的原始母行复制 indent 和未知字段；不是在正向遍历数组时原地插入。
WriteBack 不执行脚本，也没有发布后回调。

## 3. 一次发布

全部候选完成并验证后，ATT 一次替换整个 `write_back/`。任何失败都会让上一次成功
输出原样保留，半成品不会被当作成功结果。发布结果无法确认时按
[目录发布规格](../runtime/directory-publishing.md)保留现场并报告恢复位置。

`write_back/` 是一棵内容树，而不是可以直接运行的完整游戏包。把它按实际部署方式
放进隔离游戏副本、确认游戏真正读取了这些文件，这一步由操作者完成。

发布完成后按[翻译验收指南](../guides/acceptance.md)检查全部输出差异、源语残留、布局风险、
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
