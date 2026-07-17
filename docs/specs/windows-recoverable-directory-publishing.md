# Windows 可恢复目录发布现行规格

本文定义 `SystemFileSystem` 在 Windows x64 MSVC 进程中提供的文件访问与
`RecoverableDirectoryPublisher` 行为。该根把一个完整目录候选作为单一发布对象，
为 Init 的首次创建和 Standard WriteBack 的整体替换提供跨进程线性化、进程崩溃后
恢复及有界资源占用。

## 1. 平台与路径边界

发布目标、同级 stage、backup、journal 和锁文件必须位于本机固定、非大小写敏感的
NTFS 卷。根从卷根开始逐组件以不跟随 reparse point 的方式打开路径，并在同步操作
期间持有无删除共享句柄。路径链中任一符号链接、junction、mount point 或其他
reparse point 都会使操作失败；最终对象通过 Win32 volume serial 与 128 位 file ID
复核身份。

Windows 名称按系统的序数、忽略大小写语义比较。以下名称不进入候选或发布协议：

- 大小写等价的同目录重名；
- 尾点、尾空格、设备名、控制字符和保留符号；
- ADS 语法；
- 根保留的 stage、backup、journal 命名空间。

递归复制只接受普通目录和普通文件的主数据流。候选新建文件继承发布目标父目录的
ACL；根不复制 ACL、ADS、hardlink 身份和时间戳。

## 2. 普通文件能力

`ExistingDirectoryResolver`、`DirectoryLister` 和 `FileReader` 共享固定线程数、
有界队列的文件工作池。相对路径在方法调用时以进程当前工作目录转换为绝对路径；
worker 不重新解释调用方的当前目录。

- Resolver 返回逐组件校验后的规范绝对目录；
- Lister 只列举直接子项，逐项固定并拒绝在枚举后被替换成的 reparse point，且受
  单目录条目数限制；
- Reader 在分配前读取已固定文件句柄的长度，受完整文件字节上限约束，并返回原始
  字节与规范绝对路径。

工作进入有界队列前可以被取消；工作池一旦接管任务，即使等待结果的 Future 被丢弃，
任务仍执行到明确终态。worker panic 被隔离为根错误，其他 worker 继续服务。

## 3. 候选请求与唯一终结令牌

`DirectoryStageRequest` 在构造边界固定以下事实：

```text
target_root
publish_intent = CreateNew | ReplaceExisting
source_mappings[]
overlays[]
empty_directories[]
```

来源映射、覆盖和空目录使用严格相对 Windows 路径，并在请求构造时拒绝重复、重叠、
绝对路径和父级逃逸。overlay 必须落在一个来源映射内。

`prepare(request)` 先取得同目标跨进程锁并恢复该目标的已知残留，再在目标同级建立
私有候选。来源目录、overlay 和空目录共同计入以下外部配置预算：

- 同时保留的候选数；
- 候选总条目数、最大深度和总字节数；
- 单文件字节数；
- 复制来源单目录条目数；
- 单目标恢复产物数和目标锁等待时间。

成功返回的 `StagedDirectory` 不可复制，记录 publisher 实例身份、操作 UUID、父目录
和候选 file ID、发布意图、候选容量许可及同目标锁。`publish(token)` 与
`discard(token)` 按值消费；token 只能交还创建它的同一 publisher 实例。调用方在
收到 token 后直接丢弃属于内部契约错误，根仍会尽力清理未发布的候选。

`StagedDirectory` 允许受信非根服务在候选中建立后续产物，例如 Init 在复制完成后
建立 `project.db`。因此 `publish` 在任何可见交换前必须重新枚举完整候选，把后续产物
一并纳入条目、深度、单文件和总字节预算，并重新拒绝 reparse point、非普通对象、
共享同一物理文件身份的硬链接和 Windows 等价重名。复核失败时返回 `NotAttempted` 并
精确清理候选，最终目标不变。

## 4. CreateNew

`CreateNew` 不预检后宣称成功。根使用 Win32 无覆盖 handle rename 把已完成候选移动到
最终名称，该 rename 是同名并发创建的线性化点：

- 目标不存在时，候选成为目标；
- 任意同名对象已经存在时返回 `TargetAlreadyExists`；
- 其他失败保持 `NotAttempted`、`NotPublished` 或 `OutcomeUnknown` 的真实语义。

该意图绝不覆盖已有文件或目录。

## 5. ReplaceExisting 与 journal

`ReplaceExisting` 要求目标是现存目录。目标缺失返回 `TargetMissing`，目标不是目录或
是 reparse point 返回 `TargetNotDirectory` 或对应根错误。切换始终在同目标跨进程锁内
执行：

```text
写入并 sync OriginalMoveIntent
target -> backup
追加并 sync CandidateMoveIntent
stage -> target
追加并 sync CandidateVisible
复核新 target file ID
清理 backup
删除 journal
```

journal 每帧为 `u32 length + JSON payload + CRC32`。每次追加后执行 `sync_data`。
完整但 CRC、JSON、操作 ID、文件身份或阶段序列无效的帧属于损坏；只有文件末尾的
不完整帧可以回退到最后一个完整帧。journal 只保存完成恢复所需的目标名称、old/new
file ID、操作 UUID 和阶段，不用路径展示文本充当身份。

两次目录 rename 之间允许目标名称短暂缺失；根不会把逐文件半成品暴露为最终目录。

## 6. 按目标恢复

恢复不在进程启动时扫描全部项目。下一次针对同一目标执行 `prepare` 时，在取得相同
目标锁后，根据 journal、目标、stage 和 backup 的 file ID 分类：

- old 尚未移动：保留 old，清理未发布候选和 journal；
- old 位于 backup 且 new 仍在 stage：把 old 恢复为 target；
- new 已位于 target：保留 new，继续清理 backup 和 journal；
- 目标暂时缺失但 old/new 身份与位置仍可证明：返回 `RecoveryRequired` 并保留证据；
- 出现外来 file ID、损坏 journal、第三方占位对象或无法证明的组合：返回
  `OutcomeUnknown`，不猜测、不删除证据、不自动重试。

无 journal 的私有 stage 可以在持锁状态下清理；无 journal 的 backup 不能猜测其
归属。stage、backup、journal 作为一个集合计入单目标恢复产物上限。

候选、备份和 journal 的清理不使用“先校验路径、再按路径删除”。根先用拒绝
reparse 的句柄固定整条父路径和待删除对象，读取 file ID，然后用含
`DELETE` 权限的句柄再次复核同一身份并执行 disposition。目录的每个子项依次按
同一规则清空，最后才删除空目录本身。任一路径被外来 file ID 替换、变成 reparse
point、被不共享删除的句柄占用，或在枚举期间新增子项时，清理显式失败并保留证据，
绝不退回对同名路径的递归删除。

## 7. 发布终态

根向消费方保留以下互斥含义：

- `TargetAlreadyExists`：CreateNew 的目标名称已经被占用；
- `TargetMissing`：ReplaceExisting 没有现存目标；
- `TargetNotDirectory`：ReplaceExisting 的目标不是可替换目录；
- `NotAttempted`：尚未开始可见目录交换；
- `NotPublished`：候选未成为目标，原目标已按 file ID 恢复并复核；
- `PublishedWithResiduals`：新目标已经生效，但 backup、journal 或清理产物残留；
- `RecoveryRequired`：old/new 身份已知，目标当前需要下一次同目标操作继续恢复；
- `OutcomeUnknown`：无法可靠确认目标身份或可用状态。

`PublishedWithResiduals` 不是完全成功，`RecoveryRequired` 和 `OutcomeUnknown` 不触发
调用方重试或额外清理。已知未发布终态携带实际残留路径；显式 `discard` 失败携带准确
stage 路径。

## 8. 生命周期与耐久边界

FileSystem shutdown 先停止准入，再排空已接管的普通文件、候选准备、发布、恢复和清理
工作，最后 join 固定 worker。shutdown 不在中途遗弃已经接管的目录副作用。

候选文件内容和每个 journal 帧在确认前执行 `sync_data`，发布过程使用同卷 Win32
handle rename。契约保证正常 Win32 故障和进程崩溃后能够依据持久 journal 恢复；不承诺
任意硬件、控制器或文件系统在突然断电下具有绝对耐久性。
