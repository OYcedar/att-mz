# RPG Maker WriteBack 现行规格

```text
att mv write-back --name NAME [--layout-rules FILE]
att mz write-back --name NAME [--layout-rules FILE]
```

WriteBack 读取冻结来源、当前提取资产和译文，执行确定性 recipe 物化，并在项目工作区生成：

```text
<att-dir>/projects/<mv|mz>/<name>/write_back/
```

原游戏与冻结来源在整个过程中保持原样。
模型译文由 [Translate](translation.md) 生成，特殊数据库脚本由[项目数据库 Lua](../lua/README.md)执行；
WriteBack 使用已经提交的当前正文完成候选构建和发布。

## 1. 候选构建

显式提供 `--layout-rules FILE` 时，ATT 先校验并保存本次规则，再构建输出；省略时复用项目
保存的规则。规则的保存事务与正文读取分开，输出排版不会回写数据库译文。规则生命周期见
[WriteBack 排版规则](../translation/write-back-layout-rules.md#1-文件结构与生命周期)。

ATT 从冻结来源建立完整内容树，并按 recipe 把当前译文写回对应 RPG Maker 值。人工译文
优先于自动译文。自动正文的当前适用性指纹必须与当前源文、完整 Group 来源语境、项目语言对、
位置、角色和写回结构精确匹配，写回前再独立执行当前 Placeholder 和结构强校验；两项都
成立才会写回。Client、Profile、Prompt、术语和语言检查阈值不参与适用性判断。源语言残留仍只是一项
Review，不会拒绝候选或阻止 WriteBack。

正文处理顺序为：可选自动译文标点修复、规则命中的自动排版、独立续行补空白，再按 recipe
物化。两个正文开关及正式默认见[配置规格](../runtime/configuration.md#4-writeback-正文开关)；
排版文件、选择器、持久化和错误语义见
[WriteBack 排版规则规格](../translation/write-back-layout-rules.md)。

上述处理和最终物化都使用源文已经建立的 Placeholder binding 保护候选，不要求同一规则在
译文标签或自然语言上下文再次命中。已还原正文的绑定及新增自定义 Placeholder、内建控制
检查遵循 [Placeholder 规格](../translation/placeholders.md#候选验收)；数量、顺序、固定槽、
wrapper 或保留 token 不合法时不生成候选。

- 关闭标点修复时，标点逐字采用数据库译文；开启时按配置规格保留正常中文标点和完整配对的
  样式，只修复符合条件的已有自动译文标点，不增删字符或复制原文空白；人工译文不做标点修复；
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
在项目中，不发布，也不在 WriteBack 时删除；绑定事实恢复后，原状态可以重新匹配。
Partial 项目同样可以生成候选，结果会明确报告使用译文和保留原文的数量。

冻结来源包含标准 NW.js 启动文档时，WriteBack 还把唯一
`System.json.gameTitle` Unit 的当前输出同步到两个明确的派生消费者：原
`package.json.window.title` 为非空原 `gameTitle` 时只替换该 JSON string 值；活动
`package.main` HTML 中实际且唯一的小写无属性 `<title>` 元素内容逐字等于同一原值时
只替换其内容。原
`gameTitle` 为空、消费者为空或消费者已有不同标题时，保留对应的冻结字节。
该同步不增加 Unit，也不扩大到其他 HTML、package 字段或插件参数。

可换行位置与宽度由排版规则明确选择。语言、术语、措辞和运行时显示风险由译后 QA 生成
Review；需要修订时按自然 ID 使用
`manual export --ids`、check 和 apply，再重新 WriteBack。

## 2. 完整验证

发布前重新检查：

- 来源、owner 与 Mutation Claim 指纹；
- 每个 recipe 的原值、目标位置和结构；
- 修改范围没有重复、祖先/后代或跨 owner 冲突；
- 全部 JSON、事件命令、数组形状和控制符有效；
- 除上述同源启动标题消费者外，未声明位置与冻结来源逐字一致。

候选必须重新解析，并证明改动只落在受管范围内。新增 `401/405` 时，ATT 重建完整事件列表，
按每个输出行的原始母行复制 indent 和未知字段，保持后续命令的对应关系。
WriteBack 的生命周期在完整候选验证、发布并记录唯一终态后结束；操作者随后按第 3 节部署内容树
并完成实际加载验收。发布终态直接交还操作者，所需脚本由操作者在命令返回后从外部启动；
WriteBack 不注册或触发发布后回调。

## 3. 一次发布

全部候选完成并验证后，ATT 按[目录发布规格](../runtime/directory-publishing.md)一次替换整个
`write_back/`（进入目录交换前失败或取消时保持上一次输出；交换开始后的恢复与终态处理见目录发布规格）。

每次命令写 `publication.started` 和唯一 `publication.finished`：成功时保存使用译文和保留原文的
Unit 数；失败时 `result` 为 `not_published`、`recovery_required` 或 `outcome_unknown`，具体问题由
同次可读 `diagnostic.publication` 说明。

`write_back/` 是待部署的译后内容树，操作者将其部署到隔离游戏副本后，按[翻译验收指南](../guides/acceptance.md)完成检查。
