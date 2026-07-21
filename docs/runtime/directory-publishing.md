# Windows 文件能力与可恢复目录发布现行规格

## 1. 边界

`SystemFileSystem` 提供普通读取、目录树指纹、通用独占文件租约、受控候选编辑和可恢复目录发布。根只理解文件、目录、路径范围、物理身份、锁和资源预算，不理解项目、RPG Maker、`data/js`、`www` 或项目日志记录格式。

不对 `projects.root` 做全局 NTFS 品牌检查。普通读取和指纹只验证当前操作需要的普通文件、稳定身份、reparse/hardlink 拒绝与预算。独占租约在实际取得锁时验证锁能力；发布器在真实 prepare/publish 时验证同卷、handle rename、稳定 file ID 与恢复能力。

## 2. 普通文件与树指纹

Resolver、Lister、Reader 和 Fingerprinter 共享固定工作线程与有界队列：

- Resolver 返回不跟随 reparse point 的规范绝对目录；
- Lister 只列直接子项，拒绝 reparse、非普通对象、hardlink 和身份替换，并限制条目数；
- Reader 在分配前检查固定句柄长度，完整读取受单文件预算保护；
- Fingerprinter 按 Windows UTF-16 相对名的稳定顺序，把类型、空目录、文件长度和完整字节写入有 framing 的 SHA-256；绝对路径、时间戳、ACL 和 ADS 不参与。

工作一旦进入文件队列便执行到明确终态，等待 Future 被丢弃不撤销已接管操作。

## 3. 通用独占文件租约与 RPG Maker 映射

根契约接收一个锁目录和一个不透明 identity，生成稳定锁文件名并取得跨进程独占锁。它不解释项目名称。

RPG Maker 的 `ProjectCommandLeaseService` 只负责按当前 `engine` 选择固定锁目录，并把受信 `ProjectName` 作为不透明语义 identity 交给通用文件根：

```text
ProjectName → <projects.root>/.att-locks/projects/<engine> + 不透明 identity
            → 通用文件根按 Windows 非大小写敏感语义规范化
            → 稳定 SHA-256 摘要文件名 → 独占文件锁
```

因此 RPG Maker 领域不拥有锁文件名算法，通用根也不理解项目或版本；两者只在“锁目录 + identity”的契约处交接。

工作区固定在 `<projects.root>/<engine>/<project-name>`，目录发布器的目标锁位于
`<projects.root>/.att-locks/directory-publish/<engine>/`，其中 `engine` 是 `mz | mv`。
同一版本同一项目的四命令互斥，不同版本的同名项目可以并行；
固定锁序是项目租约、目录发布锁、SQLite/session。锁文件不进入会被 Init 替换的项目
工作区。命令不搜索另一版本的工作区或锁目录。

## 4. 通用候选编辑

候选编辑根接收：

```text
候选物理身份
安全相对路径
调用方声明的可编辑顶层集合
```

根负责拒绝绝对路径、父级逃逸、ADS、越出声明范围、删除声明根、reparse、hardlink、对象身份变化和预算超限。根不内置任何顶层名称。

MZ WriteBack 声明 `{data, js}`；MV WriteBack 声明 `{www}`，并由 Host 把 Lua 的逻辑
`data/js` 路径映射到 `www` 内。领域边界分别验证 MZ 顶层精确等于 `data/js`，以及 MV
顶层精确等于 `www` 且其中精确等于 `data/js`。通用文件根不需要理解这些名称。

## 5. 候选与终结令牌

`DirectoryStageRequest` 包含目标、`CreateNew | ReplaceExisting`、来源映射、overlay 和空目录。所有候选路径必须是安全相对路径；来源、覆盖、空目录的重叠与预算在 prepare 前拒绝。

prepare 先按来源目录项的 Windows UTF-16 稳定顺序建立确定性 manifest，再物化候选。
manifest 固定完整来源清单、目录与文件物理身份、普通对象类型、文件大小、hardlink 状态、
overlay 对应关系和替换后的完整树预算；每个 overlay 必须精确对应唯一的既有来源普通
文件。物化时重新核对来源目录清单，并在处理每个文件前后复核其身份、大小和链接状态。
未覆盖文件稳定读取来源字节、写入候选并 `sync_data`；被 overlay 覆盖的来源文件仍接受
相同身份、类型、大小和预算约束，但不先复制原字节，最终 overlay 字节只建立、写入并
`sync_data` 一次。

prepare 取得同目标跨进程锁、恢复已知残留，并在 target 同父目录建立 stage。任一准备失败都统一返回 `NotPrepared`，同时保留目标、首因及可选候选清理失败。成功 token 不可复制，只能由同一 publisher 按值 `publish` 或 `discard`。stage、backup 和 journal 位于 target 同父目录以保证同卷切换；RPG Maker 映射的锁文件单独位于 `<projects.root>/.att-locks/directory-publish/<engine>/`。

当前产品一次命令只拥有一个候选。树条目、深度、总字节、单文件和单目标恢复产物预算继续存在。

发布前重新枚举完整候选，纳入后续数据库产物和 Lua 编辑，并再次验证 file ID、普通对象、Windows 等价名称、reparse、hardlink 和预算。当前版本的顶层结构校验在这一通用复核之前由 WriteBack 自己完成。

## 6. CreateNew 与 ReplaceExisting

CreateNew 使用无覆盖 handle rename 作为同名创建线性化点，目标已经存在返回 `TargetAlreadyExists`，绝不覆盖。

ReplaceExisting 在同目标锁内执行：

```text
写入并刷盘 OriginalMoveIntent journal
target → backup
写入并刷盘 CandidateMoveIntent
stage → target
写入并刷盘 CandidateVisible
复核新 target 身份
清理 backup 与 journal
```

journal 使用长度、严格 JSON payload 和 CRC32 framing。只有最终不完整帧可以回退；完整损坏帧、外来身份或第三方占位导致 `OutcomeUnknown`，不得猜测清理。

下一次同目标操作按 journal 与 file ID 恢复：old 未移动则保留 old；old 在 backup 且 new 在 stage 时恢复 old；new 已成为 target 时保留 new 并继续清理；身份已知但目标暂缺返回 `RecoveryRequired`；无法归类返回 `OutcomeUnknown`。

## 7. 终态与生命周期

发布终态固定为：

- `TargetAlreadyExists`；
- `TargetMissing`；
- `TargetNotDirectory`；
- `NotAttempted`；
- `NotPublished`；
- `PublishedWithResiduals`；
- `RecoveryRequired`；
- `OutcomeUnknown`。

只有完全成功表示目标已经生效且无残留。`PublishedWithResiduals` 明确表示新目标已生效；`RecoveryRequired`/`OutcomeUnknown` 保留现场，不由调用方猜测、重试或删除。

shutdown 停止新准入，排空已接管的文件、候选、恢复和清理工作。契约保证正常 Win32 故障和进程崩溃后能够依据 journal 恢复，不承诺任意硬件断电下绝对耐久。

WriteBack 只在 publisher 返回明确终态后产生对应的类型化运行事件。普通项目日志是否成功
接收或写出该事件，不改变上述终态、恢复现场、运行方案保存条件或进程退出码；恢复只
依据 journal、目录物理身份与项目数据库。
