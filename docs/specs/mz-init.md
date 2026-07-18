# MZ 项目初始化现行规格

Init 是命名工作区的状态收敛命令。它既能建立项目，也能用同一条命令重新导入
来源、调整语言或布局并修复可判定的工作区偏差；不增加 update、repair 或 reset
命令。

## 1. 输入、租约与结果

一次调用提交项目名、现存 MZ 游戏根、源语言、目标语言，以及对话正文、滚动文本
和帮助说明三个区域的正整数宽度。进程先取得项目租约，再观测目标工作区；同名项目
的 Init、Extract、Translate 与 WriteBack 互斥，不同项目可以并行。

目标不存在时，Init 建立：

```text
<projects_root>/<name>/
├─ project.db
├─ source/
│  ├─ data/
│  └─ js/
└─ write_back/
   ├─ data/
   └─ js/
```

目标存在时，`project.db` 必须可读、属于同名项目并严格符合当前受管 schema；否则
技术失败，绝不覆盖或重建为空数据库。Init 在候选中复制并对账数据库；来源内容、
语言、布局和必需工作区结构都与输入一致时丢弃候选并返回 `Unchanged`，不修改可见
数据库，也不清空既有输出。

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

新数据库在一个初始化事务中建立：

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
  还允许 WriteBack 可恢复目录发布器留下的 `.att-dirpub-locks` 基础设施目录。
  该目录存在时，内部必须精确只有名为 `write_back` 的锁文件；命名空间可缺省，
  但不得包含其他锁名或额外对象；
  `project.db` 与这些 SQLite sidecar 必须是单链接普通文件，`source`、`write_back`
  及两者的 `data`、`js`、`.att-dirpub-locks` 必须是普通目录，锁文件也必须是
  单链接普通文件。目录列举根拒绝 reparse point、hardlink 和其他非普通对象。
  Init 只允许这些精确的 SQLite sidecar 与发布锁事实，不解释其内容。其他额外项、
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
prepare 完整工作区候选
   ↓
建立候选来源指纹
   ↓
在 candidate/project.db 创建或复制数据库并对账
   ├─ 目标原本存在、结构完整且数据库无变化 → discard → Unchanged
   └─ 需要创建、更新或修复                  → publish 一次 → Created / Updated
```

数据库准备失败只对尚未发布的候选调用一次 `discard`；清理失败与首因同时保留。
publish 按值消费候选，无论返回何种发布终态，Init 都不得再次 discard。成功只在
`Unchanged` 的候选已成功 discard，或 Created/Updated 的完整候选已经成为目标后
成立。

目录根继续区分 `TargetAlreadyExists`、`TargetMissing`、`TargetNotDirectory`、
`NotAttempted`、`NotPublished`、`PublishedWithResiduals`、`RecoveryRequired` 和
`OutcomeUnknown`，业务层不降级或自动重试。

## 6. 依赖与完成边界

```text
InitService
├─ ProjectOperationLeaseProvider
├─ ExistingDirectoryResolver
├─ SourceSnapshotFingerprint + SystemFileSystem as DirectoryTreeFingerprinter
├─ ProjectDatabaseStateReconciliationService
└─ ProjectWorkspaceConvergenceService
   ├─ ProjectWorkspaceLayout
   ├─ SqliteDatabaseCreator / SQLite backup 与短事务根
   └─ RecoverableDirectoryPublisher
```

锁顺序固定为项目租约 → 目录发布锁 → SQLite；同项目租约超时返回 `ProjectBusy`。
命令、候选终结及全部根 shutdown 成功后才呈现 Created、Updated 或 Unchanged 的
实际结果；工作区修复属于 Updated。
