# MZ 项目初始化现行规格

Init 是命名工作区的状态收敛命令。它既能建立项目，也能用同一条命令重新导入
来源、调整语言或布局并修复可判定的工作区偏差；不增加 update、repair 或 reset
命令。

## 1. 输入、租约与结果

项目名和现存 MZ 游戏根始终必填；源语言、目标语言及三个布局宽度可以逐项省略。
首次创建时五项全部必需，缺失时一次报告完整列表，并且不解析游戏目录、不准备候选、
不建库、不发布。已有项目省略单项表示复用数据库当前事实，显式项表示本次更新值；
没有任何默认语言或默认宽度。`run_started` 持久化后进程取得项目租约，再观测工作区。

目标不存在时，Init 建立：

```text
<projects.root>/mz/<name>/
├─ project.db
├─ source/
│  ├─ data/
│  └─ js/
└─ write_back/
   ├─ data/
   └─ js/
```

目标存在时，`project.db` 必须可读、属于同名项目并严格符合当前受管 schema；否则
技术失败，绝不覆盖或重建为空数据库。Init 先在可见工作区严格检查结构、数据库、
冻结来源与本次游戏来源指纹和有效设置；全部一致时直接返回 `Unchanged`，不调用
prepare、数据库 snapshot/reconcile、discard 或 publish，也不清空既有输出。

原游戏路径只服务本次收敛，不进入 metadata。后续命令只使用工作区内的冻结来源。

## 2. 来源指纹与开启边界

`source_snapshot_fingerprint` 是固定 32 字节 SHA-256。哈希覆盖 `source/data` 与 `source/js`
的完整普通目录树：

- 相对名称以 Windows UTF-16 代码单元写入，并按稳定 Windows 名称顺序遍历；
- 每项写入目录/文件类型，空目录本身也进入指纹；
- 文件写入长度和全部原始字节；
- 每个领域、字段和长度都使用无歧义 framing。

绝对路径、时间戳、ACL、ADS 和其他未复制的文件属性不参与指纹。reparse point、
hardlink、大小写碰撞、非法 Windows 名称、读取期间类型或 file ID 变化和资源超限
直接失败，不产生近似指纹。指纹连续观察两轮；摘要与按逻辑路径排列的完整对象身份
映射都必须相同，不能把“内容相同但对象已被替换”误认为稳定来源。

Init 对本次游戏来源计算期望指纹。其他三个命令每次开启项目时都重新计算冻结来源
的实际指纹并与 metadata 比较；不相等表示工作区来源已在 Init 之外变化，命令失败并
要求重新 Init。只有 Init 可以通过一次工作区整体发布更新权威来源和 metadata
指纹。

## 3. 项目数据库当前结构

MZ 项目数据库的 schema、布局、对账与开启能力属于 `att_mz::project_database`，底层
SQLite 执行机制保持共享。新数据库在一个初始化事务中建立：

- `metadata` 唯一保存 `name`、`source_language`、`target_language`、
  `dialogue_max_fullwidth_chars`、`scrolling_text_max_fullwidth_chars`、
  `help_description_max_fullwidth_chars` 和 32 字节 `source_snapshot_fingerprint`；
- `standard_asset_owner_state` 以 `owner` 为主键，owner 只允许 `builtin`、`rules`、`lua`，并保存
  该 owner 最近成功提取时的 32 字节来源指纹；存在记录即表示该 owner active；
- `standard_translation_resource` 永远且仅有 `terminology`、`placeholder_rules` 两行
  `resource_kind + canonical_json`，初始 JSON 均为 `[]`；
- `entry`、`system_text`、`map_text`、`plugin_param` 的列固定为 `owner`、
  `exact_location`、`group_location`、`field_name`、`original_text`、`translation`、
  `translation_state`；
- `text_body` 在 `field_name` 后额外保存 `unit_type`，取值只允许 `dialogue`、
  `choices`、`scrolling_text`、`event_command`；
- 五张表都以 `(owner, exact_location)` 为主键并外键引用
  `standard_asset_owner_state(owner) ON DELETE CASCADE`；`translation` 与固定 32 字节
  `translation_state` 必须同时为 NULL 或同时存在。

`source_language` 与 `target_language` 只保存规范 `LanguageId`。CLI 输入在进入项目
事实前执行 RFC 5646 解析、IANA 注册表校验和 canonicalization；合法大小写变体会被
规范化，首尾空白、下划线、非法或未注册子标签以及主语言 `und` 被拒绝。读取到非法
或非规范 metadata 表示当前项目数据库损坏，不静默修正。

删除 owner 状态会级联删除该 owner 的标准叶。受管 schema 由 Init 一次完整建立，
只包含上述 metadata、owner state、固定资源和五张标准表。

## 4. 收敛规则

既有有效数据库按输入事实确定性转换：

- 来源变化：候选使用本次游戏的完整 `data/js`，metadata 写入新指纹；所有 active
  owner 保留自身上次提取指纹，因此自然成为 stale，已有叶与译文暂时保留供下一次
  Extract 按身份继承；
- 源语言或目标语言变化：清除五表全部标准译文及 state，把 `terminology` 重置为
  `[]`；`placeholder_rules` 保持不变；
- 三个布局宽度变化：只更新对应 metadata，不改资产、译文、state 或资源；
- 工作区根的直接业务对象必须恰为 `project.db`、`source`、`write_back`，
  `source` 与 `write_back` 的直接子项都必须恰为 `data`、`js`。根目录允许
  数据库检查后留下的 `project.db-journal`、`project.db-wal`、`project.db-shm`；
  `project.db` 与这些 SQLite sidecar 必须是单链接普通文件，`source`、`write_back`
  及两者的 `data`、`js` 必须是普通目录。目录列举根拒绝 reparse point、hardlink
  和其他非普通对象。项目锁固定在 `<projects.root>/.att-locks/projects/mz/`，目录发布
  锁固定在 `<projects.root>/.att-locks/directory-publish/mz/`，两者都不属于
  工作区结构。其他额外项、
  缺失项或角色类型偏差都使用本次来源和已检查的有效数据库在候选中恢复；
- 多项同时变化时在同一个候选中组合执行，不能暴露中间状态。

任何真实变化或修复都会把候选的 `write_back/data` 与 `write_back/js` 建为空目录，
避免既有输出冒充当前状态；只有完全 `Unchanged` 保留既有输出。

## 5. 候选与单次发布

Init 不直接修改可见工作区。目标不存在时准备 `CreateNew` 候选；目标存在时从有效
数据库状态建立 `ReplaceExisting` 候选，在候选中完成来源复制、数据库转换和空输出
目录，然后只发布一次：

```text
项目租约
   ↓
检查现有 DB/结构并解析逐项有效设置
   ↓
比较当前冻结来源与本次游戏来源指纹
   ├─ 全部一致 → Unchanged（零候选操作）
   └─ 有变化/损坏 → prepare 完整工作区候选
   ↓
在 candidate/project.db 创建或复制数据库并对账
   ↓
publish 一次 → Created / Updated
```

数据库准备失败只对尚未发布的候选调用一次 `discard`；清理失败与首因同时保留。
publish 按值消费候选，无论返回何种发布终态，Init 都不得再次 discard。成功只在
快速检查确认 `Unchanged`，或 Created/Updated 的完整候选已经成为目标后成立。

目录根继续区分 `TargetAlreadyExists`、`TargetMissing`、`TargetNotDirectory`、
`NotAttempted`、`NotPublished`、`PublishedWithResiduals`、`RecoveryRequired` 和
`OutcomeUnknown`，业务层不降级或自动重试。

## 6. 依赖与完成边界

```text
InitService
├─ ProjectCommandLeaseService
│  └─ SystemFileSystem as ExclusiveFileLeaseProvider
├─ MzAuditLedger
│  └─ JsonLinesEventLog（通用追加、轮转与 sync_data）
├─ ExistingDirectoryResolver
├─ SourceSnapshotFingerprint + SystemFileSystem as DirectoryTreeFingerprinter
├─ att_mz::project_database::ProjectDatabaseStateReconciliationService
└─ ProjectWorkspaceConvergenceService
   ├─ ProjectWorkspaceLayout
   ├─ SqliteDatabaseCreator / SQLite backup 与短事务根
   └─ RecoverableDirectoryPublisher
```

锁顺序固定为项目租约 → 目录发布锁 → SQLite；同项目租约超时返回 `ProjectBusy`。
`run_started` 与 `run_finished` 进入统一 `audit.jsonl`。命令、候选终结、审计及全部
根 shutdown 成功后才呈现 Created、Updated 或 Unchanged；工作区修复属于 Updated。
MZ 只定位 `<projects.root>/mz/<name>`，不搜索其他工作区候选位置。
