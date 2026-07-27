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

WriteBack 的 Lua 选择属于项目运行方案：

- 项目从未成功保存 WriteBack 方案且省略 `--lua` 时，本次执行 Standard-only；成功后保存
  “不启用 Lua”的方案；
- 已有保存方案时，省略 `--lua` 精确复用上次成功选择；若方案启用 Lua，则执行数据库中
  保存的主程序正文；
- 显式提供非空 Lua 文件时，本次启用 Lua，并以文件正文、SHA-256 与无损 Windows 解析路径
  精确替换旧的 WriteBack Lua 程序；
- 显式提供零字节 Lua 文件时，本次执行 Standard-only，并清除 WriteBack Lua 主程序；Lua
  私有数据库状态不由核心猜测或删除。

路径只用于 chunk 名、`require` 搜索目录和诊断；主程序主动加载的模块、文件与进程仍是
可信 Lua 的动态外部依赖，不随主程序进入快照。

## 2. 读取与发布前不变量

项目开启边界先读取 metadata；Standard Reader 随后在同一个只读数据库视图中读取活动
owner 状态、`standard_text_group/unit`、`standard_mutation_claim` 冲突摘要和译文。
Reader 从每组的 kind、location 与 recipe 重建完整逻辑 Mutation Claim，随后重算每个
owner 的资产快照指纹，并验证：

- owner 的来源指纹等于项目当前来源；
- group recipe、语义单元、源上下文和重建出的完整逻辑 Mutation Claim 构成同一完整快照；
- Claim 可从 recipe 重建，且 Intent/Exclusive 资源锁在组内、owner 内、跨 owner 均无冲突；
- 重建集合按每个 `(owner, resource)` 折叠后的确定性摘要与
  `standard_mutation_claim` 逐行一致；Exclusive 必须唯一，多个 Intent 的代表必须是
  自然顺序最早的 group；
- 译文与 translation state 成对存在；
- 当前源文档仍能逐字满足 recipe 记录的命令形状、长度、值和嵌套 JSON 边界。

`translation_state` 的非空长度和与译文成对存在等合法性由当前项目数据库 schema 的
`CHECK` 约束负责；WriteBack Reader 消费已经满足该约束的行，不在读取阶段建立第二套
同义合法性规则。

任一事实不成立都在修改或发布前失败。WriteBack 不读取外部 TOML，也不重新执行 PCRE2；
姓名、局部文本和嵌套 JSON 的选择均以 Extract 时物化的 recipe 为权威。
`standard_mutation_claim` 是必须与 recipe 相符的跨 owner 冲突摘要，不是缓存，也不能
代替完整逻辑 Claim 重建、旧指纹复算或发布前冲突验证。

WriteBack Reader 与 Extract、Translate 共用同一个 group kind / role / content 结构校验：
对话、选项、滚动文本专属组只接受各自角色，其余组只接受 Scalar；Scalar Value 保留
内部 LF，DialogueSpeaker Value 拒绝 CR/LF，所有 Value 拒绝 NUL，Lines 每个元素拒绝
CR/LF/NUL。WriteBack 自己继续负责纯空白、空 Lines、Choices/ScrollingText 行数和空槽
一致性以及 recipe 引用完整性；这些写回规则不反向进入共享结构校验器。

## 3. 普通值与结构化 recipe

Scalar recipe 选择译文或原文，按记录的解码边界反向编码，并只修改对应物理目标。
未翻译单元保持原文；Literal 和结构外壳逐字保留。任何源 Scalar `Value` 本身含 LF 时，
Planner 都将其标记为 `free line breaking`；Actor profile 及 Skills、Items、Weapons、
Armors description 即使源值只有一行也使用该形状。WriteBack 把这类 Scalar 的模型行以
`\n` 连接成同一个 `Value` 后写回，其余单行 Scalar 保持 `single line`。
所有候选修改先在
内存中完成形状验证，再写入目标文档；同一文档只在全部关联修改成功后编码。

`plugins.js` 的声明、合法空白/行注释前缀、assignment、终止符和数组根由 Extract 与 Lua
共用的外壳契约解释。插件参数发生修改时，声明之前的合法前缀按 UTF-8 原始字节逐字保留，
只把 `var $plugins = ...;` 主体按当前数组重新编码；外壳失败会明确区分声明、前缀、
assignment、终止符与根类型。普通 Value 始终按完整冻结原文验收和替换；其中的裸 `<`、
`>` 或插件私有语法逐字保存，不触发局部扫描、拼接或额外候选限制。

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
生成的续行可以增加全角缩进；换行搜索预先为每条续行保留两个显示 cell，确保加上一个
全角缩进后仍不超过所选区域宽度。半角 `(`、`)` 与全角括号使用相同的配对、行尾 opener
和行首 closer 禁则。`inserted_line_breaks` 只统计这类新增物理换行。没有满足宽度、配对
和行首/行尾禁则的安全断点时返回 `Manual` 诊断，不硬切单词或 grapheme；这仍是正常写回
结果，ATT 会写入当前有效译文而不添加强制换行，并把该项计入人工处理数量，命令本身
可以成功。

## 5. Lua 候选能力

本次运行方案启用的 Lua 在 Standard 成功后运行，获得公共
`ctx.project/json/source/rpg_maker/db`、
候选专属 `ctx.output` 和 `ctx.write_back`；`extract`、`translation`、`llm` 为 nil。
`ctx.write_back.layout` 复用 Rust 布局器。

Lua 使用严格逻辑 `data/...` 与 `js/...` 访问候选；只接受 `/`，拒绝反斜杠、空段、点段、
重复/尾随分隔符、冒号和控制字符，并逐字核对目录项大小写。MV Host 在边界映射到候选的
`www/`，MZ 直接映射到顶层；脚本从不使用逻辑 `www/...`。`ctx` 不提供受管的 validate、discard 或 publish 接口；通过 `ctx.output`
进行的编辑只作用于本次未发布候选。可信 Lua 仍有除本机动态模块装载入口外的 Lua 5.4
标准库；直接执行文件系统或进程操作不属于 ATT 的候选发布契约。Lua 显式提交的数据库
事务不随 candidate discard 回滚，因此脚本必须自己拥有这类副作用的协议。

每次 WriteBack 使用新 Lua VM 和新 SQLite 连接；globals、TEMP 表和连接状态不从 Extract
或 Translate 继承。没有发布后回调。脚本必须以冻结来源、当前候选和持久私有状态重建
幂等结果，不能在 Lua 返回前把私有表标记为“已经发布”。完整 output 操作矩阵与范例见
[Lua 技术参考](lua.md#11-writeback-候选与布局)和
[Lua Cookbook](lua-cookbook.md#4-幂等-writeback)。

Lua 若拥有插件私有 grammar，必须在这里重新读取并复核完整原 Value，使用自己的已验收
状态重建完整新 Value，再通过 `ctx.output` 写回。Host 随后只验证候选目录、JSON 与
RPG Maker 公共结构，不猜测或代替脚本验收插件 grammar。完整 `<Help:...>` 示例见
[Lua 私有标签协议](lua-cookbook.md#5-插件标签的三阶段私有协议)。

## 6. 顶层验证、发布与完成边界

Standard 与可选 Lua 完成后，领域边界无条件验证候选：

- MZ 顶层恰好是普通 `data/` 与 `js/`；
- MV 顶层恰好是普通 `www/`，且 `www` 内恰好是普通 `data/` 与 `js/`。

随后通用发布器只做一次完整候选复核，覆盖普通对象、Windows 等价名称、reparse、
稳定 file ID，并按[共享文件能力的唯一契约](../runtime/directory-publishing.md#11-硬链接拒绝)
拒绝硬链接；不对文件数、深度或字节数设置 ATT 人工上限。被改写文档以合法 JSON 重新
编码；未成为 mutation Claim 的字段和未知命令字段保持来源值。候选 manifest 拥有稳定
ordinal，未改写文件复制、overlay 写入和不同物理文档改写作为独立文件任务并行执行；
完成顺序不改变主错误。Standard 后的 Lua 继续编辑同一不可见候选，随后进行上述唯一
完整校验和唯一目录交换。

目录交换未生效且 publisher 已确认 `NotPublished` 时，顶层报告项目暂时不可用；它不
表示数据库损坏或提取过期。已经生效、需要恢复或结果未知的终态继续分别保留其更强的
失败语义，不自动重试发布。

目录发布、候选终结及全部必要非日志根完成后，命令才用最后一个短 SQLite 事务替换整套
WriteBack 运行方案。业务失败、取消或必要收尾失败不更新方案；事务确认回滚时旧方案保持
不变并退出 1；若提交终态无法确认，则退出 1，明确提示输出结果与方案状态无法确认，并
建议下次显式传参。输出已经生效但方案保存失败时，诊断必须明确区分“结果已生效”和
“运行方案未保存”，不能伪装成普通成功。

项目日志只接收闭集结构化事件。启动、写入或关闭故障明确显示日志路径、操作和安全的
底层原因，但不阻止候选验证、发布或方案保存，也不改变退出码。日志从不作为恢复依据。

实时进度只报告已经建立的真实事实：资产读取、规划与文档改写在分母可得时显示局部计数，
Lua、候选验证和目录发布使用阶段 spinner。局部达到 `N/N` 后仍进入“正在收尾/保存运行
方案”，必要业务操作全部完成后才显示成功。合作取消在 publish 前丢弃候选；publish 开始
后切换为安全停止并等待唯一终态，最后保留已确认计数。
