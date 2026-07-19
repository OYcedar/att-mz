# RPG Maker 写回现行规格

WriteBack 从冻结来源重建一次完整候选，把所有活动 owner 的已确认译文应用到候选，
可选执行可信 Lua，验证后只发布一次。MZ 与 MV 共用读取、修改、排版、复核和发布流程；
差异只有工作区相对布局。

## 1. 命令与候选布局

```text
att --config FILE mz write-back --name NAME [--lua SCRIPT_LUA]
att --config FILE mv write-back --name NAME [--lua SCRIPT_LUA]
```

候选每次从 `source` 原始字节完整复制，不在旧 `write_back` 上增量修改：

```text
MZ candidate/data + candidate/js
MV candidate/www/data + candidate/www/js
```

命令先取得 `<projects.root>/.att-locks/projects/<engine>/` 中的项目租约，打开
`<projects.root>/<engine>/<name>`，验证冻结来源指纹，再准备对应引擎的目录发布候选。
任何失败发生在 publish 之前时都显式 discard 候选；publish 已开始后等待唯一明确终态。

## 2. 读取与发布前不变量

Standard Reader 在一致视图中读取 metadata、活动 owner 状态、
`standard_text_group/leaf/target` 和译文。读取后重算每个 owner 的资产快照指纹，并验证：

- owner 的来源指纹等于项目当前来源；
- group recipe、逻辑叶、翻译上下文和 Mutation Target 构成同一完整快照；
- 物理目标没有跨 group 或 owner 冲突；
- 译文与 translation state 成对存在；
- 当前源文档仍能逐字满足 recipe 记录的命令形状、长度、值和嵌套 JSON 边界。

任一事实不成立都在修改或发布前失败。WriteBack 不读取外部 TOML，也不重新执行 PCRE2；
姓名、局部文本和嵌套 JSON 的选择均以 Extract 时物化的 recipe 为权威。

## 3. 普通值与结构化 recipe

Scalar recipe 选择译文或原文，按记录的解码边界反向编码，并只修改对应物理目标。
未翻译叶保持原文；没有形成逻辑叶的 Literal 和空白内容逐字保留。所有候选修改先在
内存中完成形状验证，再写入目标文档；同一文档只在全部关联修改成功后编码。

修改摘要统计逻辑叶。物理 Mutation Target 不是译文，也不计入 translated/original
叶数。

## 4. 统一对话写回

每个标准消息块由一个 `ReplaceDialogueMutation` 处理，不再分别修改 Speaker 与 Body。
处理顺序固定为：

1. 验证完整原始 `101 + 401*` 块及 recipe；
2. 分别为可选 Speaker 与每个 `DialogueBody(n)` 选择译文或原文；
3. 只对 Body 执行对应布局宽度的换行；
4. 用冻结 Literal 和 SpeakerSlot 重建第一行姓名外壳；
5. 保持每个原始 `body[n]` 的硬边界，生成该叶对应的一条或多条 `401`；
6. 一次性替换整个命令块。

因此同一实现覆盖：无姓名无正文译文、仅姓名、仅正文、姓名与正文都有译文。一个 Body
叶可以因硬换行或自动布局扩展为多条 `401`，但不同 Body 叶绝不合并。整条 MV 第一行
只有 Speaker 时不会凭空建立 Body；MZ 的 direct Speaker target 与 MV 的 SpeakerSlot
最终都进入同一个完整块替换。

任一 Speaker/Body target、命令 code、参数形状、块长度或嵌套目标不符合 recipe 时，
该 mutation 失败且候选中不留下半修改。

滚动文本同样保持逐叶硬边界并只排版正文；其块级 mutation 覆盖完整 `105 + 405*`，
没有逻辑叶的空白 `405` 使用冻结段原样写回。帮助说明等 Scalar 根据自身布局角色选择
宽度；Speaker、选择项名称、控制符外壳和普通标量不会错误套用对话正文布局。

## 5. Lua 候选能力

显式 Lua 在 Standard 成功后运行，获得公共 `ctx.project/json/source/rpg_maker/db`、
候选专属 `ctx.output` 和 `ctx.write_back`；`extract`、`translation`、`llm` 为 nil。
`ctx.write_back.layout` 复用 Rust 布局器。

Lua 使用逻辑 `data/...` 与 `js/...` 访问候选；MV Host 在边界映射到候选的 `www/`，MZ
直接映射到顶层。脚本不能取得 publish 能力，只能修改本次未发布候选。Lua 显式提交的
数据库事务不随 candidate discard 回滚，因此脚本必须自己拥有这类副作用的协议。

## 6. 顶层验证与强审计发布

Standard 与可选 Lua 完成后，领域边界无条件验证候选：

- MZ 顶层恰好是普通 `data/` 与 `js/`；
- MV 顶层恰好是普通 `www/`，且 `www` 内恰好是普通 `data/` 与 `js/`。

随后通用发布器复核完整文件清单、普通对象、Windows 等价名称、reparse、hardlink、
file ID、文件数、深度和字节预算。被改写文档以合法 JSON 重新编码；未成为 mutation
target 的字段和未知命令字段保持来源值。

publish 前先持久化 `write_back_publish_started`；只有意图确认后才调用 publisher。
终态写入 `write_back_publish_finished`，包含 `engine`、实际布局、输出根、逻辑叶摘要、
布局诊断和 `lua_executed`。发布已生效但终态审计失败时不回滚输出，而是报告“状态已
生效但审计未确认”。

目录交换未生效且 publisher 已确认 `NotPublished` 时，顶层报告项目暂时不可用；它不
表示数据库损坏或提取过期。已经生效、需要恢复或结果未知的终态继续分别保留其更强的
失败语义，不自动重试发布。

命令、候选终结、非审计根 shutdown、`run_finished` 和审计 writer shutdown 全部成功
后才报告完成。合作取消在 publish 前丢弃候选；publish 开始后等待终态。
