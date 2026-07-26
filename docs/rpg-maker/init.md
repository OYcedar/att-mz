# RPG Maker 项目初始化现行规格

RPG Maker 是本规格所属领域，ATT 当前只支持 MZ 与 MV。Init 是两个受支持版本命名
工作区的状态收敛命令；它们使用同一套项目、数据库、来源指纹与发布能力，版本切片只
负责确认游戏目录布局，并把已经验证的内容根交给共享实现。

## 1. 命令与游戏目录

<!-- att-example: illustrative -->
```text
att --config FILE mz init --name NAME [--path GAME_ROOT] \
  [--source-language LANG] [--target-language LANG] \
  [--dialogue-max-fullwidth-chars COUNT] \
  [--scrolling-text-max-fullwidth-chars COUNT] \
  [--help-description-max-fullwidth-chars COUNT]

att --config FILE mv init --name NAME [--path GAME_ROOT] \
  [--source-language LANG] [--target-language LANG] \
  [--dialogue-max-fullwidth-chars COUNT] \
  [--scrolling-text-max-fullwidth-chars COUNT] \
  [--help-description-max-fullwidth-chars COUNT]
```

- MZ 的 `GAME_ROOT` 必须直接包含普通 `data/`、`js/` 和 `js/rmmz_core.js`；
- MV 的 `GAME_ROOT` 必须直接包含普通 `www/`，且其中包含 `data/`、`js/` 和
  `js/rpg_core.js`；
- 两者都只接受游戏根，不探测另一种受支持布局，也不把传入的 `www` 自动修正为 MV 游戏根；
- 项目名始终必填。首次 Init 的游戏根、源语言、目标语言及三个布局宽度全部必填；
- 已有项目省略 `--path` 时复用上次成功 Init 保存的来源路径，省略任一语言或宽度时复用
  metadata 中的当前事实；ATT 会明确提示这些值来自项目状态，而不把它们称作默认值；
- 显式路径或设置逐项替换本次事实。项目从未保存成功 Init 路径时，省略 `--path` 是带
  修复建议的输入错误。

项目按版本身份分开定位，因此同名项目可以共存：

<!-- att-example: illustrative -->
```text
<projects.root>/mz/<name>/
<projects.root>/mv/<name>/
```

工作区完整保留对应版本的相对布局：

<!-- att-example: illustrative -->
```text
MZ                              MV
<project>/source/data           <project>/source/www/data
<project>/source/js             <project>/source/www/js
<project>/write_back/data       <project>/write_back/www/data
<project>/write_back/js         <project>/write_back/www/js
```

工作区结构只要求上述必需文件和目录各自以正确类型存在；工作区根以及 `source`、
`write_back` 布局层级中的其他条目在完整性检查时忽略，不会单独触发重建。非权威的
`logs/` 与 `task-records/` 可观测性目录不参与来源指纹，也不会使内容相同的后续 Init
被误判为项目变化。

来源变化触发工作区重建时，这两个目录连同其全部内容被原样带入候选并随发布保留:
它们是既有运行历史与人工补译审计材料，不随 ReplaceExisting 丢弃。保留复制失败按
候选构建失败处理——候选被丢弃、现有工作区保持不变，并报告具体失败路径与原因。真正
执行 ReplaceExisting 时，候选只包含当前必需结构、本次冻结来源、数据库以及这两个
保留目录；其他未知条目不复制，随旧工作区替换而丢弃。

原游戏路径不进入 metadata，也不供 Init 之外的后续项目命令读取；它以无损 Windows
原始路径保存于 `init_run_plan`，只供后续 Init 复用。其他命令始终读取工作区内的冻结
来源。

## 2. 收敛、租约与来源指纹

命令先取得对应版本身份与项目名的租约，再观测工作区并解析显式输入或保存方案。目标
不存在时建立完整项目；目标存在时，`project.db` 必须属于同名项目并严格符合当前
schema，否则作为无效项目数据库失败。项目租约覆盖方案读取、业务执行和最终方案替换，
防止并发 Init 混用路径。

`source_snapshot_fingerprint` 是冻结内容树的 SHA-256。MZ 覆盖 `data/js`，MV 覆盖
`www/data/js`；哈希按稳定 Windows 名称顺序覆盖目录、文件长度与原始字节，并拒绝
reparse point、hardlink、名称碰撞、读取中对象身份变化和资源超限。Init 对传入游戏计算
期望指纹；Extract、Translate、WriteBack 和独立 Lua 每次开启项目时重新确认冻结来源仍
与 metadata 一致。只有 Init 可以替换来源和权威指纹。

若现有工作区、来源、语言与布局全部一致，Init 直接返回 `Unchanged`，不建立候选，
也不清空既有输出。任何真实变化或可判定结构修复都在一个未发布候选内完成，并把
`write_back` 重建为空的对应版本布局；候选只发布一次。

## 3. 当前项目数据库

项目数据库使用单一 schema，当前核心结构为：

- `metadata`：项目名、规范源/目标 `LanguageId`、三个布局宽度和来源指纹；
- `standard_asset_owner_state`：`builtin | rules | lua` 的来源指纹与资产快照指纹；
- `standard_text_group`：逻辑组、owner 内连续 `group_order`、组角色、compact canonical
  `group_location` 和完整投影/写回 recipe；
- `standard_text_unit`：组内连续 `unit_order`、compact canonical `unit_role`、源内容 JSON、
  源上下文、译文内容 JSON 与状态；
- `standard_mutation_claim`：每个 `(owner, resource)` 至多一行的确定性跨 owner 冲突摘要；
  `resource_key` 使用 compact canonical JSON，`exclusive` 保留唯一声明，多个 `intent`
  保留自然顺序最早的 group 代表；
- `standard_translation_resource`：术语与自定义占位符的 canonical JSON；
- `standard_project_definition`：活动 MV 对话定义的 canonical JSON。MZ 使用同一结构，
  但不消费 MV 姓名投影。
- `init_run_plan`：上次成功 Init 的无损 Windows 来源路径；
- `extract_run_plan` 与 `extract_rules_definition`：上次成功 Extract 的完整 owner 集合和
  已验证 Rules canonical 语义；
- `translate_run_plan`：上次成功 Translate 的 Profile；
- `write_back_run_plan`：上次成功 WriteBack 是否启用 Lua；
- `lua_program`：按 `extract | translate | write_back` 分开的非空 Lua 主程序正文、
  SHA-256 与无损 Windows 解析路径。

完整逻辑 Mutation Claim 不等于 `standard_mutation_claim` 的行集合。它由 group kind、
location 和 recipe 确定，参与组内、owner 内、跨 owner 冲突验证和 owner 资产指纹；
`standard_mutation_claim` 只持久化足以执行跨 owner 检查的确定性摘要。WriteBack 会从
recipe 重建完整逻辑 Claim、重算原 owner 指纹，并把重建出的摘要与表中内容逐行比对。
摘要因此是受检验的持久状态，不是可省略的缓存或跳过 recipe 验证的捷径。

位置、Mutation resource、unit role 和 recipe 的持久表示只接受当前 compact canonical
JSON 字节。语义等价但含额外空白、替代转义或其他非规范表示的值按普通无效项目状态处理。

语义译文身份是 `owner + group_location + unit_role`，不等于物理 JSON 地址；顺序字段也
不进入身份。删除 owner 状态会级联删除该 owner 的组、单元和 Claim 摘要。owner 状态同时
绑定来源与完整资产快照，翻译和写回均据此拒绝提取后发生的项目或资产变化。

数据库有效性只由上述当前结构、约束和领域不变量决定；不符合时按具体 schema、状态或
完整性错误处理。

检查现存项目数据库时，schema version、受管 schema、metadata、owner state、翻译资源、
项目定义、运行方案、Lua 程序、`quick_check` 与 `foreign_key_check` 全部在同一个
READ_ONLY 连接和同一个显式只读事务中按上述自然顺序读取。快照开始后即使其他连接向
WAL 提交，检查也不会拼接不同时点的项目事实；后续对账仍以快照中的精确 schema version
和领域事实执行 CAS。

来源变化保留既有 owner 快照与译文，直到下一次对应 Extract 用当前来源原子替换；语言
对变化清除所有标准译文与 state，并把术语表重置为空，保留占位符定义。布局宽度变化
只更新 metadata。

## 4. 发布与完成边界

Init 不直接修改可见工作区。创建、更新或修复都在候选中完成来源复制、数据库建立或
对账和空输出布局，然后通过可恢复目录发布器切换一次。数据库准备失败只丢弃未发布
候选；发布已经开始后必须等待明确终态，不猜测回滚。

锁顺序固定为：

<!-- att-example: illustrative -->
```text
项目租约 → 对应版本的目录发布锁 → SQLite
```

锁分别位于 `<projects.root>/.att-locks/projects/<engine>/` 和
`<projects.root>/.att-locks/directory-publish/<engine>/`。业务成功且候选终结及全部必要的
非日志根 shutdown 完成后，最后一个短 SQLite 事务才替换 `init_run_plan`。确认提交失败
时旧方案保持；提交终态无法确认时命令明确报告结果已生效但方案状态未知，并要求下次
显式传参。普通项目日志的建立、写入、flush、sync 或关闭失败不参与这个完成边界，也不改变
退出码。

实时进度只显示“检查项目、扫描来源、构建候选、数据库收敛、发布、保存运行方案”等
真实阶段 spinner，不制造无法证明的全局百分比。最终摘要在运行方案保存完成后才报告
`Created`、`Updated` 或 `Unchanged`。
