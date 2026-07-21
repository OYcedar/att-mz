# RPG Maker 写回现行规格

WriteBack 从冻结来源重建一次完整候选，把所有活动 owner 的已确认译文应用到候选，
可选执行可信 Lua，验证后只发布一次。RPG Maker 领域当前只支持 MZ 与 MV；两者共用
读取、修改、排版、复核和发布流程，差异只有工作区相对布局。

## 1. 命令与候选布局

<!-- att-example: illustrative -->
```text
att --config FILE mz write-back --name NAME [--lua SCRIPT_LUA]
att --config FILE mv write-back --name NAME [--lua SCRIPT_LUA]
```

候选每次以 `source` 完整树为权威基线重新建立，不在旧 `write_back` 上增量修改：

<!-- att-example: illustrative -->
```text
MZ candidate/data + candidate/js
MV candidate/www/data + candidate/www/js
```

命令先取得 `<projects.root>/.att-locks/projects/<engine>/` 中的项目租约，打开
`<projects.root>/<engine>/<name>`，验证冻结来源指纹，再准备对应版本的目录发布候选。
任何失败发生在 publish 之前时都显式 discard 候选；publish 已开始后等待唯一明确终态。

## 2. 读取与发布前不变量

项目开启边界先读取 metadata；Standard Reader 随后在同一个只读数据库视图中读取活动
owner 状态、`standard_text_group/unit`、`standard_mutation_claim` 和译文。读取后重算每个 owner 的资产
快照指纹，并验证：

- owner 的来源指纹等于项目当前来源；
- group recipe、语义单元、源上下文和 Mutation Claim 构成同一完整快照；
- Claim 可从 recipe 重建，且 Intent/Exclusive 资源锁在组内、owner 内、跨 owner 均无冲突；
- 译文与 translation state 成对存在；
- 当前源文档仍能逐字满足 recipe 记录的命令形状、长度、值和嵌套 JSON 边界。

任一事实不成立都在修改或发布前失败。WriteBack 不读取外部 TOML，也不重新执行 PCRE2；
姓名、局部文本和嵌套 JSON 的选择均以 Extract 时物化的 recipe 为权威。

## 3. 普通值与结构化 recipe

Scalar recipe 选择译文或原文，按记录的解码边界反向编码，并只修改对应物理目标。
未翻译单元保持原文；Literal 和结构外壳逐字保留。自由断行的 Scalar 把模型行以 `\n`
连接后写回；当前只包括 Actor profile 及 Skills、Items、Weapons、Armors description。
所有候选修改先在
内存中完成形状验证，再写入目标文档；同一文档只在全部关联修改成功后编码。

修改摘要统计语义单元。物理 Mutation Claim、模型返回行和自动生成的物理续行都不是
新的翻译单元。

## 4. 统一对话写回

每个标准消息块由一个 `ReplaceDialogueMutation` 处理，不再分别修改 Speaker 与 Body。
处理顺序固定为：

1. 验证完整原始 `101 + 401*` 块及 recipe；
2. 为可选 Speaker 与完整 DialogueBody 选择译文或原文；
3. 保持模型语义行，只对过宽行执行兜底换行；
4. 用冻结 Literal 和 SpeakerSlot 重建第一行姓名外壳；
5. 用完整最终正文行序列重建 `401`；
6. 一次性替换整个命令块。

正文没有译文时不对正文执行布局；正文文本、空行、命令模板与未知字段逐字保留。若只有
Speaker 译文，仅按冻结投影替换 SpeakerSlot 或原生 Speaker 参数。正文有译文时，完整
构造并验证替换块后才对命令数组执行一次 splice。

正文译文可以比源 `401` 更少或更多。第 i 条模型语义行使用第 i 条源正文模板；超出源
数量时使用最后一条正文模板；自动续行沿用所属语义行模板。克隆命令只替换
`parameters[0]`，保留模板的 code、indent、其他参数和未知字段。输出减少时多余命令及其
专属未知字段随命令删除，不把它们猜测性搬到其他命令。

整条 MV 第一行只有 Speaker 时不会凭空建立 Body，Speaker-only 结构行保持独立；inline
Speaker 只附着在最终第一条正文行。MZ 只修改已经存在的原生 Speaker 参数，不给四参数
`101` 增加第五参数。

任一 Speaker/Body Claim、命令 code、参数形状、块长度或嵌套目标不符合 recipe 时，
该 mutation 失败且候选中不留下半修改。

标准写回规划整体进入命令私有 CPU 根。可以独立处理的组按权威顺序并行布局，冲突
校验、诊断和摘要仍按原顺序合并；文档写回先完成全局 Claim 冲突校验，再按物理文档
并行改写并稳定排序。任何文档在全部关联 mutation 验证完成前都不会修改候选。

一个 Choices 单元由一次原子 mutation 同时验证并更新 `102.parameters[0]` 和对应同层
`402.parameters[1]`；数量、顺序和源空槽必须保持，嵌套选项按 indent 隔离，任一标签
缺失、索引错误或原文漂移时整组不修改。

ScrollingText 的块级 mutation 覆盖完整 `105 + 405*`。模型必须返回相同语义行数并保持
空槽；每个元素使用对应源 `405` 模板，过宽元素可以额外生成物理 `405`，自动续行沿用
所属源槽的模板。帮助说明根据 help width 处理每条模型行；Actor profile 等没有已建立
宽度的字段只保留语义断行。

模型提供的未超宽行逐字保持，不合并、不重排，也不因引号状态补缩进。只有 ATT 自动
生成的续行可以增加全角缩进；`inserted_line_breaks` 只统计这类新增物理换行。没有安全
断点时返回 `Manual` 诊断，不硬切单词或 grapheme；这仍是正常写回结果，ATT 会写入当前
有效译文而不添加强制换行，并把该项计入人工处理数量，命令本身可以成功。

## 5. Lua 候选能力

显式 Lua 在 Standard 成功后运行，获得公共 `ctx.project/json/source/rpg_maker/db`、
候选专属 `ctx.output` 和 `ctx.write_back`；`extract`、`translation`、`llm` 为 nil。
`ctx.write_back.layout` 复用 Rust 布局器。

Lua 使用严格逻辑 `data/...` 与 `js/...` 访问候选；只接受 `/`，拒绝反斜杠、空段、点段、
重复/尾随分隔符、冒号和控制字符，并逐字核对目录项大小写。MV Host 在边界映射到候选的
`www/`，MZ 直接映射到顶层；脚本从不使用逻辑 `www/...`。`ctx` 不提供受管的 validate、discard 或 publish 接口；通过 `ctx.output`
进行的编辑只作用于本次未发布候选。可信 Lua 仍有完整标准库，直接执行文件系统或进程
操作不属于 ATT 的候选发布契约。Lua 显式提交的数据库事务不随 candidate discard 回滚，
因此脚本必须自己拥有这类副作用的协议。

每次 WriteBack 使用新 Lua VM 和新 SQLite 连接；globals、TEMP 表和连接状态不从 Extract
或 Translate 继承。没有发布后回调。脚本必须以冻结来源、当前候选和持久私有状态重建
幂等结果，不能在 Lua 返回前把私有表标记为“已经发布”。完整 output 操作矩阵与范例见
[Lua 技术参考](lua.md#10-writeback-候选与布局)和
[Lua Cookbook](lua-cookbook.md#3-幂等-writeback)。

## 6. 顶层验证与强审计发布

Standard 与可选 Lua 完成后，领域边界无条件验证候选：

- MZ 顶层恰好是普通 `data/` 与 `js/`；
- MV 顶层恰好是普通 `www/`，且 `www` 内恰好是普通 `data/` 与 `js/`。

随后通用发布器复核完整文件清单、普通对象、Windows 等价名称、reparse、hardlink、
file ID、文件数、深度和字节预算。被改写文档以合法 JSON 重新编码；未成为 mutation
Claim 之外的字段和未知命令字段保持来源值。未改写文件从来源稳定复制并同步；被改写文档
在 Standard 初始候选中作为 overlay 直接写入并同步一次，不先复制同一路径的来源字节
再覆盖；后续显式 Lua 仍可按其既有契约编辑该候选。

publish 前先持久化 `write_back_publish_started`；只有意图确认后才调用 publisher。
终态写入 `write_back_publish_finished`，包含 `engine`、实际布局、输出根、语义单元摘要、
布局诊断和 `lua_executed`。发布已生效但终态审计失败时不回滚输出，而是报告“状态已
生效但收尾失败”。

目录交换未生效且 publisher 已确认 `NotPublished` 时，顶层报告项目暂时不可用；它不
表示数据库损坏或提取过期。已经生效、需要恢复或结果未知的终态继续分别保留其更强的
失败语义，不自动重试发布。

命令、候选终结、非审计根 shutdown、`run_finished` 和审计 writer shutdown 全部成功
后才报告完成。合作取消在 publish 前丢弃候选；publish 开始后等待终态。
